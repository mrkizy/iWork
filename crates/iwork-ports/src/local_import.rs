//! 本机导入的发现端口（T10f 方案 A3.1、A3.4）。
//!
//! A 里只列名字：运行时在本机配了哪些 MCP 服务器、技能目录、插件。来源里的原文能离开发现这一层的只有
//! [`Name`]（产品负责人接受照原样显示的四种名字，PD-A1）；连接方式只以固定标签 [`Transport`] 出去，
//! 条目 id 是种类加序号，错误只带来源种类与行列。这里的类型没有一个字段是普通的 `String` 或路径：
//! 想让来源里多一样原文出现在列表上，得先在这里给它造一种出口类型（并先有产品负责人的例外）。

use std::fmt;
use std::future::Future;

use iwork_domain::runtime::LocalConfigKind;

/// 一个名字最多保留几个字符，多出来的截掉并加 `…`（不让上兆的键名整个塞进一帧、一行）。
pub const NAME_MAX_CHARS: usize = 256;

/// 来源里的一个名字，照原样：不做语法检查、不替换看得见的字符（产品负责人定了不过滤，PD-A1）。
/// 只做两件让它仍是一行名字的事：控制字符（Unicode Cc 类）各换成一个 `�`；超过 [`NAME_MAX_CHARS`] 个字符的截断加 `…`。
///
/// 只许在四处构造：MCP 的条目名、`env` / `headers` 的键名、技能目录名、插件键（方案 A3.1）。它只用来显示，
/// 处理过的名字不能再拿去当原始配置里的键用。`Debug` 印 `<name>`：例外管的是显示在界面上、经过后台与客户端，
/// 不包括进日志；要内容得调 [`Name::as_str`]，不要拿它打日志。
#[derive(Clone, PartialEq, Eq)]
pub struct Name(String);

impl Name {
    pub fn from_source(raw: &str) -> Name {
        let mut out = String::with_capacity(raw.len().min(NAME_MAX_CHARS * 4));
        for (i, c) in raw.chars().enumerate() {
            if i == NAME_MAX_CHARS {
                out.push('…');
                break;
            }
            out.push(if c.is_control() {
                char::REPLACEMENT_CHARACTER
            } else {
                c
            });
        }
        Name(out)
    }

    /// 名字的内容。给列表显示用；不要拿它打日志。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<name>")
    }
}

/// MCP 的连接方式。适配层把来源里的写法和一张固定的表逐字比，认不出的都是 `Other`：输出的是标签，不是原文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Stdio,
    Http,
    Sse,
    Other,
}

/// 条目 id：种类加序号，没有原文。序号是来源里的先后（JSON 里出现的次序；目录按名字的字节序），
/// 来源变了同一个 id 可能指到别的条目（方案 LA4）。`Display` 与 [`ItemId::parse`] 互为逆。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemId {
    /// `mcp:user:<n>`：用户配置里的第 n 台 MCP
    UserMcp(u32),
    /// `skill:user:<n>`：用户级技能目录里的第 n 个
    UserSkill(u32),
    /// `plugin:<n>`：插件登记文件里的第 n 个插件
    Plugin(u32),
    /// `mcp:plugin:<n>:<m>`：第 n 个插件自带的第 m 台 MCP
    PluginMcp(u32, u32),
    /// `skill:plugin:<n>:<m>`：第 n 个插件的第 m 个技能目录
    PluginSkill(u32, u32),
}

impl ItemId {
    pub const fn kind(self) -> LocalConfigKind {
        match self {
            ItemId::UserMcp(_) | ItemId::PluginMcp(..) => LocalConfigKind::Mcp,
            ItemId::UserSkill(_) | ItemId::PluginSkill(..) => LocalConfigKind::Skill,
            ItemId::Plugin(_) => LocalConfigKind::Plugin,
        }
    }

    /// 解析客户端送回的 id（`ImportLocalConfig.item_ids`）。不是上面五种形状之一的返回 `None`。
    pub fn parse(s: &str) -> Option<ItemId> {
        let parts: Vec<&str> = s.split(':').collect();
        let n = |i: usize| -> Option<u32> {
            let p = parts.get(i)?;
            // 只收十进制数字本身：不收「+1」、前导零（让 Display 与 parse 一一对应）
            if p.is_empty()
                || !p.bytes().all(|b| b.is_ascii_digit())
                || (p.len() > 1 && p.starts_with('0'))
            {
                return None;
            }
            p.parse().ok()
        };
        Some(match parts.as_slice() {
            ["mcp", "user", _] => ItemId::UserMcp(n(2)?),
            ["skill", "user", _] => ItemId::UserSkill(n(2)?),
            ["plugin", _] => ItemId::Plugin(n(1)?),
            ["mcp", "plugin", _, _] => ItemId::PluginMcp(n(2)?, n(3)?),
            ["skill", "plugin", _, _] => ItemId::PluginSkill(n(2)?, n(3)?),
            _ => return None,
        })
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ItemId::UserMcp(n) => write!(f, "mcp:user:{n}"),
            ItemId::UserSkill(n) => write!(f, "skill:user:{n}"),
            ItemId::Plugin(n) => write!(f, "plugin:{n}"),
            ItemId::PluginMcp(n, m) => write!(f, "mcp:plugin:{n}:{m}"),
            ItemId::PluginSkill(n, m) => write!(f, "skill:plugin:{n}:{m}"),
        }
    }
}

/// 一项属于谁。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// 用户自己的配置
    User,
    /// 第 `index` 个插件（序号同 [`ItemId::Plugin`]），`key` 是它在登记文件里的键
    Plugin { index: u32, key: Name },
}

/// 发现到的一项，只到名字这一层。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredName {
    pub id: ItemId,
    pub kind: LocalConfigKind,
    pub name: Name,
    pub owner: Owner,
    /// 只有 MCP 有
    pub transport: Option<Transport>,
    /// 只有 MCP 有：要填值的键名（`env` 的键名、请求头的名字），照来源里的次序；值一律不读
    pub value_keys: Vec<Name>,
    /// 只有插件有：它的技能目录数
    pub skill_dirs: u32,
}

/// 出错的是哪个来源。是种类，不是路径（路径里有用户名和用户写的目录名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceRef {
    /// 用户配置（MCP 列表所在的那个文件）
    UserConfig,
    /// 插件登记文件
    PluginRegistry,
    /// 第 n 个插件自带的 MCP 配置
    PluginMcp(u32),
    /// 用户级技能目录
    SkillsDir,
    /// 第 n 个插件的技能目录
    PluginSkillsDir(u32),
}

/// 发现失败。不带原文：没有路径，没有解析器与操作系统给的文字（解析器的文字会把看到的值原样写进去）。
/// （`Display` 手写：字段名 `source` 照方案，`thiserror` 会把这个名字当成下层错误。）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalImportError {
    Unreadable {
        source: SourceRef,
        kind: std::io::ErrorKind,
    },
    Malformed {
        source: SourceRef,
        line: usize,
        column: usize,
    },
}

impl fmt::Display for LocalImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocalImportError::Unreadable { source, kind } => {
                write!(f, "本机配置读不了：{source:?}（{kind:?}）")
            }
            LocalImportError::Malformed {
                source,
                line,
                column,
            } => write!(f, "本机配置看不懂：{source:?} 第 {line} 行第 {column} 列"),
        }
    }
}

impl std::error::Error for LocalImportError {}

impl LocalImportError {
    /// 重试也没用：文件在而读不了或形状不对，要用户或 iWork 新版本去改。
    pub fn retryable(&self) -> bool {
        false
    }
}

/// 一家运行时的本机配置来源（A 只有一个方法）。
pub trait LocalConfigSource: Send + Sync {
    /// 列出本机配了什么，只到名字这一层：不读 MCP 的命令、参数、地址和任何值，不打开技能目录里的文件。
    /// 来源里的原文能出这个函数的只有四种名字（[`Name`]）；别的只以固定标签与序号的形式出去。
    /// 一个来源文件在而读不了或看不懂，整个结果是错误，不拿读得出的那一部分冒充全部。
    fn discover_names(
        &self,
    ) -> impl Future<Output = Result<Vec<DiscoveredName>, LocalImportError>> + Send;
}
