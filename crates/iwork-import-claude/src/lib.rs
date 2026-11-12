//! Claude Code 本机配置的发现，只到名字这一层（T10f 方案 A3.1）。
//!
//! 来源表（[`ConfigPaths`] 与 [`ClaudeLocalConfig::discover`]）之外的路径一概不碰；表里的文件只取名字、`type` 与键名：
//!
//! | 来源 | 怎么碰 | 取 |
//! | --- | --- | --- |
//! | user 范围的 MCP | 读用户配置（`.claude.json`） | 顶层 `mcpServers` 每一项的名字、`type`、`env` / `headers` 的键名 |
//! | 用户级技能 | 只列 `<配置目录>/skills/`，看每个条目是不是目录 | 子目录名（不含 `synced`） |
//! | 已装的插件 | 读 `<配置目录>/plugins/installed_plugins.json`；只认 `scope` 为 `user` 的记录，对每个键第一条这样的记录的 `installPath`：只列 `skills/`、读 `.mcp.json` | 插件键、技能子目录名、自带 MCP 同第一行 |
//!
//! 原文离开这个 crate 只有两种形状：[`Name`]（四种名字，产品负责人接受照原样显示，PD-A1）与固定标签 [`Transport`]。
//! 路径、`type` 的原文、解析器与操作系统的错误文字都出不去。发现不写日志。

mod files;
mod parse;

use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};

use iwork_domain::runtime::LocalConfigKind;
use iwork_ports::local_import::{
    DiscoveredName, ItemId, LocalConfigSource, LocalImportError, Name, Owner, SourceRef, Transport,
};

pub use files::{FileKind, LocalFiles, StdFiles};

/// 用户级技能目录里厂商账号同步下来的那个子目录，不列。
const SYNCED_SKILLS: &str = "synced";

/// 来源表里的路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    /// 配置目录：环境里有 `CLAUDE_CONFIG_DIR` 就是它，没有是 `~/.claude`
    pub config_dir: PathBuf,
    /// 用户配置（MCP 列表所在）：配置目录是默认值时在家目录下（`~/.claude.json`），否则在配置目录里
    pub user_config: PathBuf,
}

impl ConfigPaths {
    /// `claude_config_dir` 是后台环境里 `CLAUDE_CONFIG_DIR` 的值（空串当作没设）。照 CLI 自己的规则按「设没设」分支，不比值：
    /// 显式设成 `~/.claude` 时用户配置也在 `~/.claude/.claude.json`（2.1.322 程序：`join(CLAUDE_CONFIG_DIR || homedir(), ".claude.json")`）。
    pub fn resolve(home: &Path, claude_config_dir: Option<&OsStr>) -> ConfigPaths {
        match claude_config_dir.filter(|d| !d.is_empty()) {
            Some(dir) => {
                let config_dir = PathBuf::from(dir);
                ConfigPaths {
                    user_config: config_dir.join(".claude.json"),
                    config_dir,
                }
            }
            None => ConfigPaths {
                config_dir: home.join(".claude"),
                user_config: home.join(".claude.json"),
            },
        }
    }

    fn skills_dir(&self) -> PathBuf {
        self.config_dir.join("skills")
    }

    fn plugin_registry(&self) -> PathBuf {
        self.config_dir
            .join("plugins")
            .join("installed_plugins.json")
    }
}

/// Claude Code 的本机配置来源。
pub struct ClaudeLocalConfig<F> {
    paths: ConfigPaths,
    files: F,
}

impl<F: LocalFiles> ClaudeLocalConfig<F> {
    pub fn new(paths: ConfigPaths, files: F) -> Self {
        Self { paths, files }
    }

    /// 发现（同步做完）。条目的次序：用户的 MCP、用户级技能，然后每个插件依次是插件本身、它的 MCP、它的技能。
    pub fn discover(&self) -> Result<Vec<DiscoveredName>, LocalImportError> {
        let mut out = Vec::new();
        let user = parse_source(
            self.read_optional(&self.paths.user_config, SourceRef::UserConfig)?,
            SourceRef::UserConfig,
            parse::user_config,
        )?;
        for (i, s) in (0u32..).zip(user) {
            out.push(mcp_item(ItemId::UserMcp(i), Owner::User, s));
        }
        for (i, dir) in
            (0u32..).zip(self.skill_dirs(&self.paths.skills_dir(), SourceRef::SkillsDir, true)?)
        {
            out.push(skill_item(ItemId::UserSkill(i), Owner::User, dir));
        }
        let registry = self.paths.plugin_registry();
        let plugins = parse_source(
            self.read_optional(&registry, SourceRef::PluginRegistry)?,
            SourceRef::PluginRegistry,
            parse::plugin_registry,
        )?;
        for (n, plugin) in (0u32..).zip(plugins) {
            let (servers, skills) = match &plugin.install_path {
                Some(root) => {
                    let mcp_src = SourceRef::PluginMcp(n);
                    let servers = parse_source(
                        self.read_optional(&root.join(".mcp.json"), mcp_src)?,
                        mcp_src,
                        parse::plugin_mcp,
                    )?;
                    (
                        servers,
                        self.skill_dirs(
                            &root.join("skills"),
                            SourceRef::PluginSkillsDir(n),
                            false,
                        )?,
                    )
                }
                None => (Vec::new(), Vec::new()),
            };
            let owner = Owner::Plugin {
                index: n,
                key: plugin.key.clone(),
            };
            out.push(DiscoveredName {
                id: ItemId::Plugin(n),
                kind: LocalConfigKind::Plugin,
                name: plugin.key,
                owner: Owner::User,
                transport: None,
                value_keys: Vec::new(),
                skill_dirs: u32::try_from(skills.len()).unwrap_or(u32::MAX),
            });
            for (m, s) in (0u32..).zip(servers) {
                out.push(mcp_item(ItemId::PluginMcp(n, m), owner.clone(), s));
            }
            for (m, dir) in (0u32..).zip(skills) {
                out.push(skill_item(ItemId::PluginSkill(n, m), owner.clone(), dir));
            }
        }
        Ok(out)
    }

    /// 读一个来源文件。不存在 → `None`（这个来源是空的）；在而读不了 → 错误，只带来源种类与 io 的错误种类。
    fn read_optional(
        &self,
        path: &Path,
        source: SourceRef,
    ) -> Result<Option<Vec<u8>>, LocalImportError> {
        match self.files.read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(LocalImportError::Unreadable {
                source,
                kind: e.kind(),
            }),
        }
    }

    /// 列一个技能目录：只列、只判是不是目录，不打开任何文件。按名字的字节序。不存在 → 空。
    fn skill_dirs(
        &self,
        dir: &Path,
        source: SourceRef,
        skip_synced: bool,
    ) -> Result<Vec<Name>, LocalImportError> {
        let unreadable = |e: io::Error| LocalImportError::Unreadable {
            source,
            kind: e.kind(),
        };
        let mut entries = match self.files.list(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(unreadable(e)),
        };
        entries.sort_by(|a, b| a.as_encoded_bytes().cmp(b.as_encoded_bytes()));
        let mut names = Vec::new();
        for entry in entries {
            if skip_synced && entry == SYNCED_SKILLS {
                continue;
            }
            match self.files.kind(&dir.join(&entry)) {
                Ok(FileKind::Dir) => names.push(dir_name(&entry)),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {} // 断了的链接：不是目录
                Err(e) => return Err(unreadable(e)),
            }
        }
        Ok(names)
    }
}

impl<F: LocalFiles> LocalConfigSource for ClaudeLocalConfig<F> {
    /// 在调用时同步做完（读几个小文件、列几个目录；方案 A7）。受理方要在异步运行时里调，自己决定要不要挪到阻塞线程。
    fn discover_names(
        &self,
    ) -> impl Future<Output = Result<Vec<DiscoveredName>, LocalImportError>> + Send {
        std::future::ready(self.discover())
    }
}

/// 出口三：技能目录名。不是 UTF-8 的部分显示成 `�`。
fn dir_name(entry: &OsString) -> Name {
    Name::from_source(&entry.to_string_lossy())
}

fn parse_source<T>(
    bytes: Option<Vec<u8>>,
    source: SourceRef,
    parse: impl FnOnce(&[u8]) -> Result<Vec<T>, parse::ParseError>,
) -> Result<Vec<T>, LocalImportError> {
    match bytes {
        None => Ok(Vec::new()),
        Some(b) => parse(&b).map_err(|e| LocalImportError::Malformed {
            source,
            line: e.line,
            column: e.column,
        }),
    }
}

fn mcp_item(id: ItemId, owner: Owner, s: parse::Server) -> DiscoveredName {
    DiscoveredName {
        id,
        kind: LocalConfigKind::Mcp,
        name: s.name,
        owner,
        transport: Some(s.transport),
        value_keys: s.value_keys,
        skill_dirs: 0,
    }
}

fn skill_item(id: ItemId, owner: Owner, name: Name) -> DiscoveredName {
    DiscoveredName {
        id,
        kind: LocalConfigKind::Skill,
        name,
        owner,
        transport: None,
        value_keys: Vec::new(),
        skill_dirs: 0,
    }
}

/// 连接方式的固定文字（给示例程序与将来的列表说明用）。
pub fn transport_label(t: Transport) -> &'static str {
    match t {
        Transport::Stdio => "stdio",
        Transport::Http => "http",
        Transport::Sse => "sse",
        Transport::Other => "其他",
    }
}

/// `discover-names` 示例程序的输出（D1 核对用）：每项一行，id、种类、名字、连接方式、键名，末尾一行条数。
/// 名字照原样印（产品负责人接受显示在界面上，D1 在产品负责人自己的终端里看）；路径、值不在里面。
pub fn render_listing(items: &[DiscoveredName]) -> String {
    let mut out = String::new();
    for d in items {
        let kind = match d.kind {
            LocalConfigKind::Mcp => "MCP",
            LocalConfigKind::Skill => "技能",
            LocalConfigKind::Plugin => "插件",
        };
        let owner = match &d.owner {
            Owner::User => String::new(),
            Owner::Plugin { key, .. } => format!("（插件 {}）", key.as_str()),
        };
        let mut line = format!("{}\t{kind}\t{}{owner}", d.id, d.name.as_str());
        if let Some(t) = d.transport {
            let keys: Vec<&str> = d.value_keys.iter().map(Name::as_str).collect();
            line.push_str(&format!(
                "\t{}\t键 {}：{}",
                transport_label(t),
                keys.len(),
                keys.join(", ")
            ));
        }
        if d.kind == LocalConfigKind::Plugin {
            line.push_str(&format!("\t{} 个技能目录", d.skill_dirs));
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&format!("共 {} 项\n", items.len()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_table() {
        use parse::transport_of;
        assert_eq!(transport_of(None), Transport::Stdio);
        assert_eq!(transport_of(Some("stdio")), Transport::Stdio);
        assert_eq!(transport_of(Some("http")), Transport::Http);
        assert_eq!(transport_of(Some("streamable-http")), Transport::Http);
        assert_eq!(transport_of(Some("sse")), Transport::Sse);
        assert_eq!(transport_of(Some("ws")), Transport::Other);
        assert_eq!(
            transport_of(Some("HTTP")),
            Transport::Other,
            "逐字比，不改大小写"
        );
    }
}
