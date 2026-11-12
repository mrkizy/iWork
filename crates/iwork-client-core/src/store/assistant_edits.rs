//! 助手配置的自动保存闸（步 7c，ui-m1.md 第 1 节「修改即时自动保存」）。
//!
//! 为什么要闸：`UpdateAssistant` 带乐观并发的 `expected_version`，客户端从侧栏摘要读这个版本，而摘要要等重查
//! 才更新。用户在这段时间里连改两项，第二次会带着旧版本被拒（CONFLICT）；拿视图拼整份配置发出去，还会把前一次
//! 改动和别的写入方改的字段一起盖掉。
//!
//! 做法：网页只说「改了哪一项」（[`Field`]），每个助手一道闸：
//! - 用户还没看到落定的改动记在 `unsettled`（同一项后改的覆盖先改的）。
//! - 同一个助手同时只放一次 `UpdateAssistant` 在途；发的配置 = 后台最新配置（摘要里的）+ `unsettled` 全部。
//!   别的写入方改的其他字段因此不会被盖掉；同一字段两边都改，用户这次的改动胜出。
//! - 受理之后，等摘要追上才算落定，再放行排队的：摘要版本超过发出时带的那个（配置变了，版本涨了），或者版本
//!   没变而摘要里的配置就是发出的那份（配置本来就是这样，后台不产生事件、版本不涨）。受理回包分不出有没有产生
//!   事件（没有事件时 first_seq = last_seq = 当时的 head），所以按摘要判断。
//! - 被拒：CONFLICT → 等摘要换了版本再按「最新配置 + 没落定的」重拼重放。别的错误（字段非法等，校验在后台，
//!   AGENTS.md「能力与界面分离」）：那一次只带一项 → 丢掉这一项、把错误留给页面，不重试（否则会一直重发）；
//!   带了好几项 → 分不出是哪一项，改成一次只带一项逐个重放（拆开发），被拒的那项丢掉，其余照常落定。
//! - 断线：在途那一次留在会话层的在飞表里按原 command_id 重发，闸不动。
//!
//! 这里只有状态与决定，不发命令；会话层按返回的 [`Send`] 发 `UpdateAssistant`。

use std::collections::BTreeMap;

use iwork_proto::pb;

/// 用户改的一项。
#[derive(Debug, Clone, PartialEq)]
pub enum Field {
    Name(String),
    Prompt(String),
    /// 模型与思考强度一起改（菜单里是同一次选择）。
    Model(pb::ModelSelection),
    /// 一个工具分组的开关。
    Tool {
        group: i32,
        enabled: bool,
    },
}

/// 一项的身份：同一项后改的覆盖先改的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FieldKey {
    Name,
    Prompt,
    Model,
    Tool(i32),
}

impl Field {
    pub fn key(&self) -> FieldKey {
        match self {
            Field::Name(_) => FieldKey::Name,
            Field::Prompt(_) => FieldKey::Prompt,
            Field::Model(_) => FieldKey::Model,
            Field::Tool { group, .. } => FieldKey::Tool(*group),
        }
    }

    /// 把这一项写进一份配置。
    pub fn apply(&self, c: &mut pb::AssistantConfig) {
        match self {
            Field::Name(v) => c.name.clone_from(v),
            Field::Prompt(v) => c.prompt.clone_from(v),
            Field::Model(m) => c.model = Some(m.clone()),
            Field::Tool { group, enabled } => {
                c.disabled_groups.retain(|g| g != group);
                if !enabled {
                    c.disabled_groups.push(*group);
                    c.disabled_groups.sort_unstable();
                }
            }
        }
    }
}

/// 后台此刻的样子（侧栏摘要里的）。
#[derive(Debug, Clone, Copy)]
pub struct Base<'a> {
    pub version: u32,
    pub config: &'a pb::AssistantConfig,
}

/// 要发的一次 `UpdateAssistant`。
#[derive(Debug, Clone, PartialEq)]
pub struct Send {
    pub assistant_id: String,
    pub config: pb::AssistantConfig,
    pub expected_version: u32,
}

/// 一项改动与它的编号（同一个助手内递增）。落定按编号认，不按值：用户在命令在途时把一项改回同一个值，
/// 期间别的写入方又改了它，这次改动得再发一次，按值比会被当成已经落定吞掉。
type Edits = BTreeMap<FieldKey, (u64, Field)>;

/// 在途的那一次。
#[derive(Debug, Clone, Default)]
struct Flight {
    expected: u32,
    /// 这一次带的改动（落定时从 `unsettled` 里去掉编号相同的：之后又改过的留着）。
    fields: Edits,
    /// 发出的整份配置（判断「版本没变、配置就是这份」的落定）。
    config: pb::AssistantConfig,
    /// 已受理：等摘要追上就落定（见 [`AssistantEdits::accepted`]）。
    accepted: bool,
}

#[derive(Debug, Clone, Default)]
struct Slot {
    unsettled: Edits,
    next_edit: u64,
    flight: Option<Flight>,
    /// CONFLICT 之后：等摘要版本不再是这个才重放（版本没变时重放只会再冲突一次）。
    stale_version: Option<u32>,
    /// 带好几项的一次被校验拒了：改成一次只带一项，找出是哪一项（没落定的都发完就回到合并发）。
    one_at_a_time: bool,
    error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AssistantEdits {
    slots: BTreeMap<String, Slot>,
}

impl AssistantEdits {
    /// 用户改了一项。`base` 是摘要里这个助手此刻的样子（找不到助手时为 `None`，什么都不发）。
    pub fn edit(&mut self, id: &str, field: Field, base: Option<Base<'_>>) -> Option<Send> {
        let slot = self.slots.entry(id.to_owned()).or_default();
        slot.next_edit += 1;
        slot.unsettled.insert(field.key(), (slot.next_edit, field));
        slot.error = None;
        self.progress(id, base)
    }

    /// 摘要里这个助手更新了（重查结果到了）：在途那次落定了就放行排队的；冲突之后版本换了就重放。
    pub fn observed(&mut self, id: &str, base: Option<Base<'_>>) -> Option<Send> {
        self.progress(id, base)
    }

    /// 在途那一次受理了：等摘要追上（版本涨过发出时的，或版本没变而配置就是发出的那份）就落定。
    pub fn accepted(&mut self, id: &str, base: Option<Base<'_>>) -> Option<Send> {
        let flight = self.slots.get_mut(id)?.flight.as_mut()?;
        flight.accepted = true;
        self.progress(id, base)
    }

    /// 在途那一次被拒。`conflict`：版本冲突（重拼重放）；否则是校验类错误（不重试，记下错误）。
    pub fn rejected(
        &mut self,
        id: &str,
        conflict: bool,
        message: &str,
        base: Option<Base<'_>>,
    ) -> Option<Send> {
        let slot = self.slots.get_mut(id)?;
        let flight = slot.flight.take()?;
        if conflict {
            slot.stale_version = Some(flight.expected);
        } else if flight.fields.len() > 1 {
            // 分不出是哪一项：拆开逐个重放
            slot.one_at_a_time = true;
        } else {
            settle(&mut slot.unsettled, &flight.fields);
            slot.error = Some(message.to_owned());
        }
        self.progress(id, base)
    }

    /// 配置页显示的样子：摘要里的配置 + 还没落定的改动（摘要要等重查才追上，不叠上去会闪回旧值）。
    pub fn shown(&self, id: &str, config: &pb::AssistantConfig) -> pb::AssistantConfig {
        let mut c = config.clone();
        if let Some(slot) = self.slots.get(id) {
            for (_, field) in slot.unsettled.values() {
                field.apply(&mut c);
            }
        }
        c
    }

    /// 保存失败的说明（校验类错误）；下一次改动清掉。
    pub fn error(&self, id: &str) -> Option<&str> {
        self.slots.get(id)?.error.as_deref()
    }

    /// 还有没落定的改动或在途的命令（会话层据此在摘要更新后叫 [`Self::observed`]）。
    pub fn busy(&self) -> impl Iterator<Item = &str> {
        self.slots
            .iter()
            .filter(|(_, s)| s.flight.is_some() || !s.unsettled.is_empty())
            .map(|(id, _)| id.as_str())
    }

    /// 助手没了（删了）：它的闸一起丢掉。
    pub fn forget(&mut self, id: &str) {
        self.slots.remove(id);
    }

    /// 能往前走就走：落定在途的、按需发下一次。
    fn progress(&mut self, id: &str, base: Option<Base<'_>>) -> Option<Send> {
        let base = base?;
        let slot = self.slots.get_mut(id)?;
        if let Some(f) = &slot.flight {
            let caught_up = base.version > f.expected
                || (base.version == f.expected && *base.config == f.config);
            if !(f.accepted && caught_up) {
                return None;
            }
            let f = slot.flight.take().expect("上面刚取到");
            settle(&mut slot.unsettled, &f.fields);
        }
        if slot.stale_version == Some(base.version) {
            return None;
        }
        slot.stale_version = None;
        if slot.unsettled.is_empty() {
            slot.one_at_a_time = false;
            return None;
        }
        let fields: Edits = if slot.one_at_a_time {
            slot.unsettled
                .iter()
                .take(1)
                .map(|(k, v)| (*k, v.clone()))
                .collect()
        } else {
            slot.unsettled.clone()
        };
        let mut config = base.config.clone();
        for (_, field) in fields.values() {
            field.apply(&mut config);
        }
        slot.flight = Some(Flight {
            expected: base.version,
            fields,
            config: config.clone(),
            accepted: false,
        });
        Some(Send {
            assistant_id: id.to_owned(),
            config,
            expected_version: base.version,
        })
    }
}

/// 去掉已经落定的改动：编号和那一次带的相同才去（之后又改过的，留着等下一次）。
fn settle(unsettled: &mut Edits, sent: &Edits) {
    unsettled.retain(|k, (n, _)| sent.get(k).map(|(m, _)| m) != Some(n));
}
