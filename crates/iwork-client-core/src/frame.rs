//! 帧引擎（T09 方案 2.4）：网页来取的那一刻才从当前状态算帧，按 256 KiB 分块一块一块交出去。
//!
//! - `apply` 只往脏集合记键；`pull` 时才 derive + 编码。网页不来取就没有数据离开状态库（R5）。
//! - 代次：`attach` 让代次加一，丢掉上一代没交完的帧，全部视图记脏；带旧代次的取帧一律「代次过期」。
//! - 取帧带上网页已应用到的 `(frame, index)`：对不上（回包丢了）就原样再交那一块；对得上交下一块；
//!   当前帧交完了，等「有脏键、且距上一帧开始 ≥ 16ms」再算新的一帧。
//! - 分块对所有 patch 一视同仁：一帧是编码后的 `ViewPatch` 字节，按上限切。任何时刻交出去没被确认的
//!   最多一块；Rust 侧另外只留当前帧没交完的字节。

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use bytes::Bytes;
use iwork_proto::pb;
use prost::Message;

use crate::derive::{self, Ctx};
use crate::store::{DirtySet, Store, ViewKey};

/// 一块的上限（方案 2.4 第 5 条）。
pub const FRAME_CHUNK_MAX: usize = 256 * 1024;
/// 两帧开始之间的最小间隔。
pub const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// 取帧的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Pull {
    Chunk(pb::FrameChunk),
    /// 带的是旧代次：不给数据。
    StaleEpoch,
    /// 现在没有可交的：有脏键时最早在 `Some(t)` 能出帧；`None` 表示等新的脏键。
    Wait(Option<Instant>),
}

#[derive(Debug)]
struct CurrentFrame {
    frame: u64,
    bytes: Bytes,
    chunks: u32,
    /// 最后交出去的那一块。
    handed: Option<u32>,
}

impl CurrentFrame {
    fn chunk(&self, epoch: u32, index: u32, max: usize) -> pb::FrameChunk {
        let start = (index as usize) * max;
        let end = (start + max).min(self.bytes.len());
        pb::FrameChunk {
            epoch,
            frame: self.frame,
            index,
            last: index + 1 == self.chunks,
            data: self.bytes.slice(start..end),
        }
    }
}

/// 网页手里某个对话已有的行（按行身份）。增量据此算：窗口里新出现的行整行发，离开窗口的行发删除。
#[derive(Debug, Default)]
struct SentRows {
    conversation_id: Option<String>,
    rows: BTreeSet<String>,
}

#[derive(Debug)]
pub struct FrameEngine {
    epoch: u32,
    next_frame: u64,
    current: Option<CurrentFrame>,
    last_frame_started: Option<Instant>,
    chunk_max: usize,
    sent: SentRows,
    /// 已交出的字节总数（测试与回退路径断言用）。
    handed_bytes: u64,
    /// 算出过的帧数。
    frames_built: u64,
}

impl Default for FrameEngine {
    fn default() -> Self {
        Self::new(FRAME_CHUNK_MAX)
    }
}

impl FrameEngine {
    pub fn new(chunk_max: usize) -> Self {
        assert!(chunk_max > 0, "分块上限必须大于 0");
        Self {
            epoch: 0,
            next_frame: 1,
            current: None,
            last_frame_started: None,
            chunk_max,
            sent: SentRows::default(),
            handed_bytes: 0,
            frames_built: 0,
        }
    }

    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    pub fn handed_bytes(&self) -> u64 {
        self.handed_bytes
    }

    pub fn frames_built(&self) -> u64 {
        self.frames_built
    }

    /// 当前帧还没交完的字节数（上界检查用）。
    pub fn held_bytes(&self) -> usize {
        self.current.as_ref().map_or(0, |c| c.bytes.len())
    }

    /// 网页（重新）挂上：换代次，丢掉上一代没交完的帧，全部视图记脏，下一帧是全量。
    pub fn attach(&mut self, store: &mut Store) -> u32 {
        self.epoch = self.epoch.wrapping_add(1);
        self.current = None;
        self.sent = SentRows::default();
        store.dirty.mark_all();
        self.epoch
    }

    /// 网页来取一块（方案 2.4 第 4 条）。
    pub fn pull(
        &mut self,
        store: &mut Store,
        epoch: u32,
        applied: Option<(u64, u32)>,
        now: Instant,
        ctx: Ctx,
    ) -> Pull {
        if epoch != self.epoch {
            return Pull::StaleEpoch;
        }
        if let Some(cur) = self.current.as_mut() {
            match cur.handed {
                Some(h) if applied != Some((cur.frame, h)) => {
                    // 回包丢了：原样再交那一块，网页按 (frame, index) 去重。
                    let c = cur.chunk(self.epoch, h, self.chunk_max);
                    self.handed_bytes += c.data.len() as u64;
                    return Pull::Chunk(c);
                }
                Some(h) if h + 1 < cur.chunks => {
                    cur.handed = Some(h + 1);
                    let c = cur.chunk(self.epoch, h + 1, self.chunk_max);
                    self.handed_bytes += c.data.len() as u64;
                    return Pull::Chunk(c);
                }
                Some(_) => self.current = None,
                None => {
                    cur.handed = Some(0);
                    let c = cur.chunk(self.epoch, 0, self.chunk_max);
                    self.handed_bytes += c.data.len() as u64;
                    return Pull::Chunk(c);
                }
            }
        }
        if store.dirty.is_empty() {
            return Pull::Wait(None);
        }
        if let Some(t) = self.last_frame_started {
            let due = t + FRAME_INTERVAL;
            if now < due {
                return Pull::Wait(Some(due));
            }
        }
        let dirty = store.take_dirty();
        let patch = self.build(store, dirty, ctx);
        if patch.changes.is_empty() {
            // 脏的全是不可见视图：丢掉，不出帧。
            return Pull::Wait(None);
        }
        self.last_frame_started = Some(now);
        self.frames_built += 1;
        let frame = self.next_frame;
        self.next_frame += 1;
        let bytes = Bytes::from(pb::ViewPatch { frame, ..patch }.encode_to_vec());
        let chunks = u32::try_from(bytes.len().div_ceil(self.chunk_max).max(1)).unwrap_or(u32::MAX);
        self.current = Some(CurrentFrame {
            frame,
            bytes,
            chunks,
            handed: None,
        });
        self.pull(store, epoch, applied, now, ctx)
    }

    /// 从脏集合算一帧：不可见的丢掉，可见的从当前状态整份算（时间线只发窗口里变了的行）。
    fn build(&mut self, store: &Store, dirty: DirtySet, ctx: Ctx) -> pb::ViewPatch {
        use pb::view_change::Change as C;
        let v = &store.visible;
        let mut changes = Vec::new();
        for key in &dirty.views {
            let change = match key {
                ViewKey::Sidebar if v.sidebar => Some(C::Sidebar(pb::SidebarPatch {
                    view: Some(derive::sidebar::sidebar(store, ctx)),
                })),
                ViewKey::Pending if v.pending => Some(C::Pending(pb::PendingPatch {
                    view: Some(derive::pending::pending(store)),
                })),
                ViewKey::Settings if v.settings => Some(C::Settings(pb::SettingsPatch {
                    view: Some(derive::settings::settings(store)),
                })),
                ViewKey::Assistant => v
                    .assistant_id
                    .as_deref()
                    .and_then(|id| derive::assistant::assistant(store, id))
                    .map(|view| C::Assistant(pb::AssistantProfilePatch { view: Some(view) })),
                ViewKey::AddProject if v.add_project => Some(C::AddProject(pb::AddProjectPatch {
                    view: Some(derive::settings::add_project(store)),
                })),
                ViewKey::Connection => Some(C::Connection(pb::ConnectionPatch {
                    view: Some(derive::connection::connection(store)),
                })),
                ViewKey::ConversationList => derive::settings::conversation_list(store, ctx)
                    .filter(|_| v.conversation_list.is_some())
                    .map(|view| {
                        C::ConversationList(pb::ConversationListPatch { view: Some(view) })
                    }),
                ViewKey::Conversation => self.conversation(store, &dirty).map(C::Conversation),
                ViewKey::Navigate => store.navigate.as_ref().map(|(seq, c, r)| {
                    C::Navigate(pb::NavigatePatch {
                        seq: *seq,
                        conversation_id: c.clone(),
                        row_key: r.clone(),
                    })
                }),
                _ => None,
            };
            if let Some(change) = change {
                changes.push(pb::ViewChange {
                    change: Some(change),
                });
            }
        }
        pb::ViewPatch { frame: 0, changes }
    }

    fn conversation(&mut self, store: &Store, dirty: &DirtySet) -> Option<pb::ConversationPatch> {
        let visible = store.visible.conversation_id.as_deref()?;
        let Some(tl) = store.open.as_ref().filter(|t| t.conversation_id == visible) else {
            // 对话被删了或还没打开：让网页清空。
            self.sent = SentRows {
                conversation_id: Some(visible.to_owned()),
                rows: BTreeSet::new(),
            };
            return Some(pb::ConversationPatch {
                conversation_id: visible.to_owned(),
                reset: true,
                ..Default::default()
            });
        };
        let (window, info) = derive::conversation::window(store, tl);
        let want: BTreeSet<String> = window.iter().map(|(_, id)| id.clone()).collect();
        let reset =
            dirty.conversation.reset || self.sent.conversation_id.as_deref() != Some(visible);
        let mut rows = Vec::new();
        let mut removed = Vec::new();
        let mut appends = Vec::new();
        for (key, id) in &window {
            let fresh =
                reset || !self.sent.rows.contains(id) || dirty.conversation.rows.contains(id);
            if fresh {
                if let Some(row) = tl.row(id) {
                    rows.push(derive::row::row(store, tl, *key, id, row));
                }
            } else if let Some(before) = dirty.conversation.appended.get(id) {
                let text = tl.text(id).unwrap_or_default();
                if let Some(tail) = text.get(*before..) {
                    appends.push(pb::MessageTextAppend {
                        message_id: id.clone(),
                        offset: *before as u64,
                        text: tail.to_owned(),
                    });
                }
            }
        }
        if !reset {
            removed.extend(self.sent.rows.difference(&want).cloned());
        }
        self.sent = SentRows {
            conversation_id: Some(visible.to_owned()),
            rows: want,
        };
        Some(pb::ConversationPatch {
            conversation_id: visible.to_owned(),
            header: Some(derive::conversation::header(store, tl)),
            appends,
            removed_ids: removed,
            window: Some(info),
            rows,
            reset,
            composer: Some(derive::conversation::composer(store, tl)),
            blocking_notice: tl
                .incomplete
                .as_ref()
                .map(|_| derive::text::INCOMPLETE.to_owned()),
            ..Default::default()
        })
    }
}
