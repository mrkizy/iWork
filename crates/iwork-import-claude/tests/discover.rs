//! 发现只到名字这一层（T10f 方案 A4 的 RA1、RA2 前三条、RA6 不经受理路径的几条、CA3）。
//!
//! 假文件接缝记下每一次调用；合成夹具里暗号分两组：名字组（四种名字，照原样显示）与别的组（值、命令、参数、地址、
//! `type`、`installPath`、技能文件、别的顶层键下的假凭据，都不许出去）。

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use iwork_domain::runtime::LocalConfigKind;
use iwork_import_claude::{
    ClaudeLocalConfig, ConfigPaths, FileKind, LocalFiles, StdFiles, render_listing,
};
use iwork_ports::local_import::{
    DiscoveredName, ItemId, LocalConfigSource, LocalImportError, Owner, SourceRef, Transport,
};

// ---------- 假文件系统 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Read,
    List,
    Kind,
}

#[derive(Default)]
struct FakeFiles {
    files: BTreeMap<PathBuf, Vec<u8>>,
    dirs: Vec<PathBuf>,
    /// 这些路径上的任何操作都报这个错误
    broken: BTreeMap<PathBuf, io::ErrorKind>,
    calls: Mutex<Vec<(Op, PathBuf)>>,
}

impl FakeFiles {
    fn file(mut self, path: &str, content: &str) -> Self {
        let p = PathBuf::from(path);
        self.add_parents(&p);
        self.files.insert(p, content.as_bytes().to_vec());
        self
    }
    fn broken(mut self, path: &str, kind: io::ErrorKind) -> Self {
        self.broken.insert(PathBuf::from(path), kind);
        self
    }
    fn add_parents(&mut self, p: &Path) {
        for a in p.ancestors().skip(1) {
            if a.as_os_str().is_empty() || self.dirs.iter().any(|d| d == a) {
                continue;
            }
            self.dirs.push(a.to_path_buf());
        }
    }
    fn calls(&self) -> Vec<(Op, PathBuf)> {
        self.calls.lock().unwrap().clone()
    }
    fn note(&self, op: Op, path: &Path) -> io::Result<()> {
        self.calls.lock().unwrap().push((op, path.to_path_buf()));
        match self.broken.get(path) {
            Some(kind) => Err(io::Error::new(
                *kind,
                format!("fake error at {}", path.display()),
            )),
            None => Ok(()),
        }
    }
}

impl LocalFiles for FakeFiles {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.note(Op::Read, path)?;
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
    fn list(&self, dir: &Path) -> io::Result<Vec<OsString>> {
        self.note(Op::List, dir)?;
        if !self.dirs.iter().any(|d| d == dir) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let mut names: Vec<OsString> = self
            .dirs
            .iter()
            .filter(|d| d.parent() == Some(dir))
            .chain(self.files.keys().filter(|f| f.parent() == Some(dir)))
            .map(|p| p.file_name().unwrap().to_os_string())
            .collect();
        names.reverse(); // 故意不按字节序给：发现自己要排
        Ok(names)
    }
    fn kind(&self, path: &Path) -> io::Result<FileKind> {
        self.note(Op::Kind, path)?;
        if self.dirs.iter().any(|d| d == path) {
            Ok(FileKind::Dir)
        } else if self.files.contains_key(path) {
            Ok(FileKind::File)
        } else {
            Err(io::ErrorKind::NotFound.into())
        }
    }
}

// ---------- 合成夹具「带暗号的元数据」 ----------

const HOME: &str = "/home/u";
const CFG: &str = "/home/u/.claude";
const P1: &str = "/home/u/.claude/plugins/cache/OTHER_INSTALLPATH_7q/p1";
const P2: &str = "/home/u/.claude/plugins/cache/p2";

/// 名字组：照原样出现在名字、键名、来源里
const NAMES: &[&str] = &[
    "https://example.invalid/mcp?token=NAME_URLTOKEN_1",
    "ghp_NAME_TOKENNAME_2",
    "Bearer NAME_HEADERKEY_3",
    "NAME_ENVKEY_4",
    "NAME skilldir 5",
    "NAME_PLUGINKEY_6/x@m",
    "NAME_PLUGINSKILL_7",
    "srv_NAME_PLUGINMCP_8",
    "NAME_ENVKEY_9",
    "NAME_WRAPPED_10",
];

/// 别的组：在任何出口都不许出现
const OTHERS: &[&str] = &[
    "OTHER_URLQUERY_1",
    "OTHER_BEARER_2",
    "OTHER_HEADERVAL_3",
    "OTHER_COMMAND_4",
    "OTHER_ARG_5",
    "OTHER_ENVVAL_6",
    "OTHER_ENVVAL_7",
    "OTHER_TYPE_8",
    "OTHER_ACCESSTOKEN_9",
    "OTHER_APIKEY_10",
    "OTHER_PROJECTENV_11",
    "OTHER_SKILLMD_12",
    "OTHER_INSTALLPATH_7q",
    "OTHER_PLUGINSKILLMD_14",
    "OTHER_PLUGINCMD_15",
    "OTHER_PLUGINENVVAL_16",
    "OTHER_PLUGINJSON_17",
    "OTHER_CREDENTIALS_18",
    "OTHER_SETTINGS_19",
    "OTHER_LOCALONLY_20",
    "OTHER_SCOPE_21",
    "OTHER_WRAPPEDURL_22",
    "OTHER_SYNCED_23",
];

fn fixture() -> FakeFiles {
    let user_config = r#"{
      "oauthAccount": {"accessToken": "OTHER_ACCESSTOKEN_9", "emailAddress": "x@example.invalid"},
      "primaryApiKey": "OTHER_APIKEY_10",
      "projects": {"/w": {"mcpServers": {"x": {"env": {"K": "OTHER_PROJECTENV_11"}}}}},
      "mcpServers": {
        "https://example.invalid/mcp?token=NAME_URLTOKEN_1": {
          "type": "http", "url": "https://h.invalid/mcp?key=OTHER_URLQUERY_1",
          "headers": {"Authorization": "Bearer OTHER_BEARER_2", "Bearer NAME_HEADERKEY_3": "OTHER_HEADERVAL_3"}
        },
        "ghp_NAME_TOKENNAME_2": {
          "command": "OTHER_COMMAND_4", "args": ["--token", "OTHER_ARG_5"],
          "env": {"NAME_ENVKEY_4": "OTHER_ENVVAL_6", "API_KEY": "OTHER_ENVVAL_7"}
        },
        "plain": {"type": "OTHER_TYPE_8"}
      }
    }"#;
    let registry = format!(
        r#"{{"version": 2, "plugins": {{
          "NAME_PLUGINKEY_6/x@m": [
            {{"scope": "local", "installPath": "/elsewhere/OTHER_LOCALONLY_20", "projectPath": "/w"}},
            {{"scope": "user", "installPath": "{P1}", "version": "1"}},
            {{"scope": "user", "installPath": "/second/user/record"}}
          ],
          "local-only@m": [{{"scope": "local", "installPath": "/elsewhere/OTHER_LOCALONLY_20"}}],
          "OTHER_SCOPE_21@m": [{{"scope": "OTHER_SCOPE_21", "installPath": "/x"}}],
          "wrapped@m": [{{"scope": "user", "installPath": "{P2}"}}],
          "no-path@m": [{{"scope": "user"}}]
        }}}}"#
    );
    FakeFiles::default()
        .file(&format!("{HOME}/.claude.json"), user_config)
        .file(&format!("{CFG}/skills/NAME skilldir 5/SKILL.md"), "secret OTHER_SKILLMD_12")
        .file(&format!("{CFG}/skills/zz-second/SKILL.md"), "secret OTHER_SKILLMD_12")
        .file(&format!("{CFG}/skills/README"), "OTHER_SKILLMD_12")
        .file(&format!("{CFG}/skills/synced/OTHER_SYNCED_23/SKILL.md"), "OTHER_SYNCED_23")
        .file(&format!("{CFG}/plugins/installed_plugins.json"), &registry)
        .file(&format!("{P1}/.claude-plugin/plugin.json"), r#"{"name": "OTHER_PLUGINJSON_17"}"#)
        .file(&format!("{P1}/skills/NAME_PLUGINSKILL_7/SKILL.md"), "OTHER_PLUGINSKILLMD_14")
        .file(&format!("{P1}/skills/NAME_PLUGINSKILL_7/scripts/run.sh"), "OTHER_PLUGINSKILLMD_14")
        .file(&format!("{P1}/.mcp.json"), r#"{"srv_NAME_PLUGINMCP_8": {"command": "OTHER_PLUGINCMD_15", "env": {"NAME_ENVKEY_9": "OTHER_PLUGINENVVAL_16"}}}"#)
        .file(&format!("{P2}/.mcp.json"), r#"{"mcpServers": {"NAME_WRAPPED_10": {"type": "sse", "url": "https://h.invalid/?OTHER_WRAPPEDURL_22"}}}"#)
        .file(&format!("{CFG}/.credentials.json"), "OTHER_CREDENTIALS_18")
        .file(&format!("{CFG}/settings.json"), "OTHER_SETTINGS_19")
        .file(&format!("{CFG}/projects/w/session.jsonl"), "OTHER_CREDENTIALS_18")
}

/// 测试与发现共用同一份假文件（记录调用）。
#[derive(Clone)]
struct Shared(std::sync::Arc<FakeFiles>);

impl LocalFiles for Shared {
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        self.0.read(p)
    }
    fn list(&self, p: &Path) -> io::Result<Vec<OsString>> {
        self.0.list(p)
    }
    fn kind(&self, p: &Path) -> io::Result<FileKind> {
        self.0.kind(p)
    }
}

/// 发现的结果与这一次的文件调用记录。
type Discovery = (
    Result<Vec<DiscoveredName>, LocalImportError>,
    Vec<(Op, PathBuf)>,
);

fn discover(files: FakeFiles) -> Discovery {
    let shared = Shared(std::sync::Arc::new(files));
    let src = ClaudeLocalConfig::new(ConfigPaths::resolve(Path::new(HOME), None), shared.clone());
    let r = pollster::block_on(src.discover_names());
    (r, shared.0.calls())
}

/// 返回值里经 `as_str()` 看得见的全部文字（名字、来源的插件键、键名）
fn shown_texts(items: &[DiscoveredName]) -> Vec<String> {
    let mut out = Vec::new();
    for d in items {
        out.push(d.name.as_str().to_string());
        if let Owner::Plugin { key, .. } = &d.owner {
            out.push(key.as_str().to_string());
        }
        out.extend(d.value_keys.iter().map(|k| k.as_str().to_string()));
    }
    out
}

/// 除了 `as_str()` 之外的每一个出口：id、Debug、示例程序的输出
fn other_outputs(items: &[DiscoveredName]) -> String {
    let ids: Vec<String> = items.iter().map(|d| d.id.to_string()).collect();
    format!("{}\n{:?}\n{}", ids.join("\n"), items, render_listing(items))
}

fn ok(files: FakeFiles) -> (Vec<DiscoveredName>, Vec<(Op, PathBuf)>) {
    let (r, calls) = discover(files);
    (r.expect("夹具应当能发现"), calls)
}

#[test]
fn names_shown_verbatim_everything_else_stays() {
    let (items, _) = ok(fixture());
    let shown = shown_texts(&items);
    for n in NAMES {
        assert!(
            shown.iter().any(|s| s == n),
            "名字组的 {n:?} 应当照原样、一个字符不差地出现在名字、来源或键名里：{shown:?}"
        );
    }
    let everything = format!("{}\n{}", shown.join("\n"), other_outputs(&items));
    for o in OTHERS {
        assert!(!everything.contains(o), "别的组的 {o:?} 出去了");
    }
    // 示例程序的输出里名字照印（D1 要看），别的组照样没有
    let listing = render_listing(&items);
    assert!(
        listing.contains("ghp_NAME_TOKENNAME_2")
            && listing.ends_with(&format!("共 {} 项\n", items.len()))
    );
}

#[test]
fn names_do_not_reach_ids_errors_or_logs() {
    let (items, _) = ok(fixture());
    let ids: Vec<String> = items.iter().map(|d| d.id.to_string()).collect();
    let debug = format!("{items:?}");
    for n in NAMES {
        assert!(!ids.iter().any(|i| i.contains(n)), "{n} 进了 id");
        assert!(!debug.contains(n), "{n} 进了 Debug");
    }
    // 错误里也没有名字：名字是暗号的那一项的值不是对象
    let bad = FakeFiles::default().file(
        &format!("{HOME}/.claude.json"),
        r#"{"mcpServers": {"ghp_NAME_INERROR": "OTHER_VALUE_INERROR"}}"#,
    );
    let err = discover(bad).0.unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(
        !text.contains("ghp_NAME_INERROR") && !text.contains("OTHER_VALUE_INERROR"),
        "{text}"
    );
}

#[test]
fn discover_names_drops_env_and_header_values() {
    let (items, _) = ok(fixture());
    let mcp: Vec<&DiscoveredName> = items
        .iter()
        .filter(|d| d.kind == LocalConfigKind::Mcp)
        .collect();
    let keys = |id: ItemId| -> Vec<String> {
        mcp.iter()
            .find(|d| d.id == id)
            .unwrap()
            .value_keys
            .iter()
            .map(|k| k.as_str().to_string())
            .collect()
    };
    // 对照：键名照原样、照来源里的次序（env 与 headers 合在一起）
    assert_eq!(
        keys(ItemId::UserMcp(0)),
        ["Authorization", "Bearer NAME_HEADERKEY_3"]
    );
    assert_eq!(keys(ItemId::UserMcp(1)), ["NAME_ENVKEY_4", "API_KEY"]);
    assert_eq!(keys(ItemId::UserMcp(2)), Vec::<String>::new());
    assert_eq!(keys(ItemId::PluginMcp(0, 0)), ["NAME_ENVKEY_9"]);
    let everything = format!("{:?} {}", shown_texts(&items), other_outputs(&items));
    for v in [
        "OTHER_BEARER_2",
        "OTHER_HEADERVAL_3",
        "OTHER_ENVVAL_6",
        "OTHER_ENVVAL_7",
        "OTHER_PLUGINENVVAL_16",
        "OTHER_PROJECTENV_11",
    ] {
        assert!(!everything.contains(v), "{v}");
    }
}

#[test]
fn discover_names_takes_only_names_and_keys_from_mcp_config() {
    let (items, calls) = ok(fixture());
    for f in [
        format!("{HOME}/.claude.json"),
        format!("{P1}/.mcp.json"),
        format!("{P2}/.mcp.json"),
    ] {
        assert!(
            calls.contains(&(Op::Read, PathBuf::from(&f))),
            "{f} 应当确实被读了"
        );
    }
    let mcp: Vec<(ItemId, &str, Option<Transport>)> = items
        .iter()
        .filter(|d| d.kind == LocalConfigKind::Mcp)
        .map(|d| (d.id, d.name.as_str(), d.transport))
        .collect();
    assert_eq!(
        mcp,
        vec![
            (
                ItemId::UserMcp(0),
                "https://example.invalid/mcp?token=NAME_URLTOKEN_1",
                Some(Transport::Http)
            ),
            (
                ItemId::UserMcp(1),
                "ghp_NAME_TOKENNAME_2",
                Some(Transport::Stdio)
            ),
            (ItemId::UserMcp(2), "plain", Some(Transport::Other)),
            (
                ItemId::PluginMcp(0, 0),
                "srv_NAME_PLUGINMCP_8",
                Some(Transport::Stdio)
            ),
            (
                ItemId::PluginMcp(1, 0),
                "NAME_WRAPPED_10",
                Some(Transport::Sse)
            ),
        ]
    );
    let everything = format!("{:?} {}", shown_texts(&items), other_outputs(&items));
    for v in [
        "OTHER_COMMAND_4",
        "OTHER_ARG_5",
        "OTHER_URLQUERY_1",
        "OTHER_PLUGINCMD_15",
        "OTHER_WRAPPEDURL_22",
        "OTHER_TYPE_8",
        "OTHER_ACCESSTOKEN_9",
        "OTHER_APIKEY_10",
    ] {
        assert!(!everything.contains(v), "{v}");
    }
}

#[test]
fn discover_names_never_reads_skill_content() {
    let (_, calls) = ok(fixture());
    for skills in [
        format!("{CFG}/skills"),
        format!("{P1}/skills"),
        format!("{P2}/skills"),
    ] {
        let under: Vec<&(Op, PathBuf)> = calls
            .iter()
            .filter(|(_, p)| p.starts_with(&skills))
            .collect();
        assert!(
            under.iter().all(|(op, _)| *op != Op::Read),
            "{skills} 之下有 read：{under:?}"
        );
    }
    assert!(
        calls.contains(&(Op::List, PathBuf::from(format!("{CFG}/skills")))),
        "对照：技能目录确实列过"
    );
    assert!(
        !calls
            .iter()
            .any(|(_, p)| p.starts_with(format!("{CFG}/skills/synced"))),
        "synced 连判断是不是目录都不做"
    );
}

#[test]
fn discover_names_touches_only_registered_paths() {
    let (_, calls) = ok(fixture());
    let mut reads: Vec<PathBuf> = calls
        .iter()
        .filter(|(op, _)| *op == Op::Read)
        .map(|(_, p)| p.clone())
        .collect();
    reads.sort();
    let mut want: Vec<PathBuf> = [
        format!("{HOME}/.claude.json"),
        format!("{CFG}/plugins/installed_plugins.json"),
        format!("{P1}/.mcp.json"),
        format!("{P2}/.mcp.json"),
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    want.sort();
    assert_eq!(
        reads, want,
        "read 过的路径恰好是来源表里的文件（没有 user 记录、没有 installPath 的插件不读）"
    );
    for untouched in [
        format!("{CFG}/.credentials.json"),
        format!("{CFG}/settings.json"),
        format!("{CFG}/projects"),
        format!("{P1}/.claude-plugin"),
        "/elsewhere".into(),
        "/second".into(),
        "/x".into(),
    ] {
        assert!(
            !calls.iter().any(|(_, p)| p.starts_with(&untouched)),
            "{untouched} 被碰了"
        );
    }
}

#[test]
fn plugins_come_from_first_user_record_only() {
    let (items, _) = ok(fixture());
    let plugins: Vec<(ItemId, &str, u32)> = items
        .iter()
        .filter(|d| d.kind == LocalConfigKind::Plugin)
        .map(|d| (d.id, d.name.as_str(), d.skill_dirs))
        .collect();
    // local-only 与范围是别的串的键不列；有 user 记录而没写 installPath 的照列，技能目录 0、没有自带 MCP
    assert_eq!(
        plugins,
        vec![
            (ItemId::Plugin(0), "NAME_PLUGINKEY_6/x@m", 1),
            (ItemId::Plugin(1), "wrapped@m", 0),
            (ItemId::Plugin(2), "no-path@m", 0)
        ]
    );
    let owned_by_2 = items
        .iter()
        .filter(|d| matches!(d.owner, Owner::Plugin { index: 2, .. }))
        .count();
    assert_eq!(owned_by_2, 0);
    let skill = items
        .iter()
        .find(|d| d.id == ItemId::PluginSkill(0, 0))
        .unwrap();
    assert_eq!(skill.name.as_str(), "NAME_PLUGINSKILL_7");
    assert!(
        matches!(&skill.owner, Owner::Plugin { index: 0, key } if key.as_str() == "NAME_PLUGINKEY_6/x@m")
    );
}

#[test]
fn user_skills_are_dirs_in_byte_order_without_synced() {
    let (items, _) = ok(fixture());
    let skills: Vec<(ItemId, &str)> = items
        .iter()
        .filter(|d| d.id.kind() == LocalConfigKind::Skill && d.owner == Owner::User)
        .map(|d| (d.id, d.name.as_str()))
        .collect();
    assert_eq!(
        skills,
        vec![
            (ItemId::UserSkill(0), "NAME skilldir 5"),
            (ItemId::UserSkill(1), "zz-second")
        ],
        "README 不是目录，synced 不列"
    );
}

#[test]
fn transport_is_a_fixed_label() {
    let cases = [
        ("", Transport::Stdio),
        (r#""type": null,"#, Transport::Stdio),
        (r#""type": "stdio","#, Transport::Stdio),
        (r#""type": "http","#, Transport::Http),
        (r#""type": "streamable-http","#, Transport::Http),
        (r#""type": "sse","#, Transport::Sse),
        (r#""type": "ws","#, Transport::Other),
        (r#""type": "OTHER_TYPE_SECRET","#, Transport::Other),
        (r#""type": 3,"#, Transport::Other),
        (r#""type": {"x": "OTHER_TYPE_SECRET"},"#, Transport::Other),
    ];
    for (field, want) in cases {
        let cfg = format!(r#"{{"mcpServers": {{"s": {{{field} "command": "c"}}}}}}"#);
        let (items, _) = ok(FakeFiles::default().file(&format!("{HOME}/.claude.json"), &cfg));
        assert_eq!(items[0].transport, Some(want), "{field}");
        assert!(!format!("{items:?} {}", render_listing(&items)).contains("OTHER_TYPE_SECRET"));
    }
}

#[test]
fn item_ids_are_kind_and_ordinals() {
    let (items, _) = ok(fixture());
    assert!(!items.is_empty());
    for d in &items {
        let text = d.id.to_string();
        assert!(
            text.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b':'),
            "{text}"
        );
        assert_eq!(ItemId::parse(&text), Some(d.id), "{text}");
        assert_eq!(d.id.kind(), d.kind);
    }
}

#[test]
fn errors_carry_no_source_text() {
    let cases: Vec<(FakeFiles, SourceRef)> = vec![
        // 畸形 JSON：出错位置紧挨着一串暗号
        (FakeFiles::default().file(&format!("{HOME}/.claude.json"), "{\"mcpServers\": {\"a\": {\"type\": \"x\"}} OTHER_ERR_NEAR_1"), SourceRef::UserConfig),
        // 名字是暗号的那一项的值是字符串暗号：serde_json 自己的错误文字里有它
        (FakeFiles::default().file(&format!("{HOME}/.claude.json"), r#"{"mcpServers": {"NAME_ERR_2": "OTHER_ERR_VALUE_2"}}"#), SourceRef::UserConfig),
        // installPath 指向读不了的路径，路径里带暗号
        (
            FakeFiles::default()
                .file(&format!("{CFG}/plugins/installed_plugins.json"), r#"{"plugins": {"p@m": [{"scope": "user", "installPath": "/opt/OTHER_ERR_PATH_3"}]}}"#)
                .broken("/opt/OTHER_ERR_PATH_3/.mcp.json", io::ErrorKind::PermissionDenied),
            SourceRef::PluginMcp(0),
        ),
        (
            FakeFiles::default()
                .file(&format!("{CFG}/plugins/installed_plugins.json"), r#"{"plugins": {"p@m": [{"scope": "user", "installPath": "/opt/OTHER_ERR_PATH_4"}]}}"#)
                .broken("/opt/OTHER_ERR_PATH_4/skills", io::ErrorKind::PermissionDenied),
            SourceRef::PluginSkillsDir(0),
        ),
    ];
    for (files, want_source) in cases {
        let err = discover(files).0.unwrap_err();
        let text = format!("{err} {err:?}");
        for secret in [
            "OTHER_ERR_NEAR_1",
            "OTHER_ERR_VALUE_2",
            "NAME_ERR_2",
            "OTHER_ERR_PATH_3",
            "OTHER_ERR_PATH_4",
            "/opt",
            "fake error",
        ] {
            assert!(!text.contains(secret), "{secret} 进了错误：{text}");
        }
        let source = match err {
            LocalImportError::Malformed {
                source,
                line,
                column,
            } => {
                assert!(line >= 1 && column >= 1, "行列在");
                source
            }
            LocalImportError::Unreadable { source, kind } => {
                assert_eq!(kind, io::ErrorKind::PermissionDenied);
                source
            }
        };
        assert_eq!(source, want_source);
    }
}

#[test]
fn server_entry_that_is_not_an_object_is_malformed() {
    // 两种写法、两个来源都一样：某一项的值不是对象 → 报错，不跳过
    for (path, body, src) in [
        (
            format!("{HOME}/.claude.json"),
            r#"{"mcpServers": {"a": {"type": "http"}, "b": ["x"]}}"#,
            SourceRef::UserConfig,
        ),
        (
            format!("{P2}/.mcp.json"),
            r#"{"a": {"command": "c"}, "b": 3}"#,
            SourceRef::PluginMcp(0),
        ),
        (
            format!("{P2}/.mcp.json"),
            r#"{"mcpServers": {"a": "x"}}"#,
            SourceRef::PluginMcp(0),
        ),
    ] {
        let files = FakeFiles::default()
            .file(
                &format!("{CFG}/plugins/installed_plugins.json"),
                &format!(
                    r#"{{"plugins": {{"p@m": [{{"scope": "user", "installPath": "{P2}"}}]}}}}"#
                ),
            )
            .file(&path, body);
        assert!(
            matches!(discover(files).0, Err(LocalImportError::Malformed { source, .. }) if source == src),
            "{body}"
        );
    }
}

#[test]
fn plugin_mcp_wrapper_needs_to_be_the_only_key() {
    let reg = format!(r#"{{"plugins": {{"p@m": [{{"scope": "user", "installPath": "{P2}"}}]}}}}"#);
    let names = |body: &str| -> Vec<String> {
        let files = FakeFiles::default()
            .file(&format!("{CFG}/plugins/installed_plugins.json"), &reg)
            .file(&format!("{P2}/.mcp.json"), body);
        ok(files)
            .0
            .iter()
            .filter(|d| d.kind == LocalConfigKind::Mcp)
            .map(|d| d.name.as_str().to_string())
            .collect()
    };
    assert_eq!(
        names(r#"{"mcpServers": {"a": {}, "b": {"type": "sse"}}}"#),
        ["a", "b"],
        "包了一层：取里面那层"
    );
    assert_eq!(
        names(r#"{"a": {}, "b": {}}"#),
        ["a", "b"],
        "顶层就是服务器表"
    );
    assert_eq!(
        names(r#"{"mcpServers": {"command": "c"}, "b": {}}"#),
        ["mcpServers", "b"],
        "mcpServers 不是唯一的键：它也只是一台服务器的名字"
    );
    assert_eq!(names(r#"{}"#), Vec::<String>::new());
}

#[test]
fn discover_names_reports_malformed_source() {
    let files = FakeFiles::default().file(
        &format!("{CFG}/plugins/installed_plugins.json"),
        r#"{"version": 2, "plugins": ["not", "a", "map"]}"#,
    );
    assert!(matches!(
        discover(files).0,
        Err(LocalImportError::Malformed {
            source: SourceRef::PluginRegistry,
            line: 1,
            ..
        })
    ));
}

/// 插件登记文件三种情形分开（T10f A 审查 1/F-1）：文件不存在 → 空；`plugins` 是空表 → 空；文件在而没有 `plugins` 这个键
/// → 形状不对，报 Malformed，不当成「没装插件」。错误里照样只有来源种类与行列。
#[test]
fn registry_without_plugins_field_is_malformed() {
    let registry = |body: &str| {
        FakeFiles::default().file(&format!("{CFG}/plugins/installed_plugins.json"), body)
    };
    assert_eq!(ok(FakeFiles::default()).0.len(), 0, "文件不存在：空");
    assert_eq!(
        ok(registry(r#"{"version": 2, "plugins": {}}"#)).0.len(),
        0,
        "合法的空表：空"
    );
    for body in [
        r#"{"version": 2}"#,
        r#"{"version": 2, "plugin": {"OTHER_TYPO_KEY@m": []}}"#,
        "{}",
    ] {
        let err = discover(registry(body)).0.unwrap_err();
        assert!(
            matches!(
                err,
                LocalImportError::Malformed {
                    source: SourceRef::PluginRegistry,
                    line: 1,
                    ..
                }
            ),
            "{body}: {err:?}"
        );
        assert!(
            !format!("{err} {err:?}").contains("OTHER_TYPO_KEY"),
            "{err}"
        );
    }
}

#[test]
fn missing_source_is_empty_not_error() {
    // 什么都没有：查询成功、列表为空
    let (items, _) = ok(FakeFiles::default());
    assert!(items.is_empty());
    // 只有用户配置：插件一组为空
    let (items, _) = ok(FakeFiles::default().file(
        &format!("{HOME}/.claude.json"),
        r#"{"mcpServers": {"a": {}}}"#,
    ));
    assert_eq!(items.len(), 1);
    // 读得到但读不了的（权限）是错误，不是空
    let broken = FakeFiles::default().broken(
        &format!("{HOME}/.claude.json"),
        io::ErrorKind::PermissionDenied,
    );
    assert_eq!(
        discover(broken).0.unwrap_err(),
        LocalImportError::Unreadable {
            source: SourceRef::UserConfig,
            kind: io::ErrorKind::PermissionDenied
        }
    );
}

#[test]
fn config_dir_from_env_or_home() {
    let home = Path::new("/home/u");
    assert_eq!(
        ConfigPaths::resolve(home, None),
        ConfigPaths {
            config_dir: "/home/u/.claude".into(),
            user_config: "/home/u/.claude.json".into()
        }
    );
    assert_eq!(
        ConfigPaths::resolve(home, Some(std::ffi::OsStr::new("/cfg"))),
        ConfigPaths {
            config_dir: "/cfg".into(),
            user_config: "/cfg/.claude.json".into()
        }
    );
    assert_eq!(
        ConfigPaths::resolve(home, Some(std::ffi::OsStr::new(""))),
        ConfigPaths::resolve(home, None),
        "空串当没设"
    );
    // 审查 1/F-2：显式设成默认目录也按「设了」走，读 $HOME/.claude/.claude.json，不读 $HOME/.claude.json。
    // 依据 2.1.322 程序：`globalConfig: join(CLAUDE_CONFIG_DIR || homedir(), ".claude.json")`，按变量有没有设分支，不比值
    assert_eq!(
        ConfigPaths::resolve(home, Some(std::ffi::OsStr::new("/home/u/.claude"))),
        ConfigPaths {
            config_dir: "/home/u/.claude".into(),
            user_config: "/home/u/.claude/.claude.json".into()
        }
    );
}

/// 真文件系统：用户的技能目录是链接时照常列（D1 的一条），断了的链接不算目录。
#[cfg(unix)]
#[test]
fn std_files_follow_a_linked_skills_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let real = home.join("real-skills");
    std::fs::create_dir_all(real.join("alpha")).unwrap();
    std::fs::write(real.join("alpha/SKILL.md"), "OTHER_REALFS_SKILLMD").unwrap();
    std::os::unix::fs::symlink(home.join("missing"), real.join("dangling")).unwrap();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::os::unix::fs::symlink(&real, home.join(".claude/skills")).unwrap();
    let src = ClaudeLocalConfig::new(ConfigPaths::resolve(home, None), StdFiles);
    let items = pollster::block_on(src.discover_names()).unwrap();
    let names: Vec<&str> = items.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["alpha"]);
}
