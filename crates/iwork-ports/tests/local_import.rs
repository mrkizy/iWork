//! 本机导入的出口类型（T10f 方案 A3.1、A3.4、RA6）。

use iwork_domain::runtime::LocalConfigKind;
use iwork_ports::local_import::{
    DiscoveredName, ItemId, LocalImportError, NAME_MAX_CHARS, Name, Owner, SourceRef, Transport,
};

#[test]
fn name_from_source_table() {
    // 看得见的字符一个不改：带 :/?=&、空格、中文、像 token 的串都照原样（产品负责人定了不过滤）
    for raw in [
        "https://example.invalid/mcp?token=abc&x=1",
        "Bearer abc",
        "中文 名字",
        "ghp_0123456789",
        "a.b_c-d@e",
        "",
    ] {
        assert_eq!(Name::from_source(raw).as_str(), raw);
    }
    // 控制字符各换成一个 �
    assert_eq!(
        Name::from_source("a\nb\tc\u{1b}[31md\u{7f}").as_str(),
        "a�b�c�[31md�"
    );
    // 长度：256 个字符的不动，257 个的截成 256 个加 …（按字符数，不按字节）
    let at_limit = "名".repeat(NAME_MAX_CHARS);
    assert_eq!(Name::from_source(&at_limit).as_str(), at_limit);
    let over = "x".repeat(NAME_MAX_CHARS + 1);
    assert_eq!(
        Name::from_source(&over).as_str(),
        format!("{}…", "x".repeat(NAME_MAX_CHARS))
    );
    assert_eq!(NAME_MAX_CHARS, 256);
}

#[test]
fn name_debug_hides_content() {
    let name = Name::from_source("ghp_secret_looking_name");
    assert_eq!(format!("{name:?}"), "<name>");
    let item = DiscoveredName {
        id: ItemId::Plugin(3),
        kind: LocalConfigKind::Plugin,
        name: name.clone(),
        owner: Owner::Plugin {
            index: 3,
            key: name,
        },
        transport: None,
        value_keys: vec![Name::from_source("API_KEY_secret_looking")],
        skill_dirs: 2,
    };
    let debug = format!("{item:?}");
    assert!(!debug.contains("secret_looking"), "{debug}");
    assert!(
        debug.contains("Plugin(3)") && debug.contains("skill_dirs: 2"),
        "对照：别的字段照常印：{debug}"
    );
}

#[test]
fn item_ids_display_and_parse() {
    let ids = [
        (ItemId::UserMcp(0), "mcp:user:0", LocalConfigKind::Mcp),
        (
            ItemId::UserSkill(12),
            "skill:user:12",
            LocalConfigKind::Skill,
        ),
        (ItemId::Plugin(3), "plugin:3", LocalConfigKind::Plugin),
        (
            ItemId::PluginMcp(3, 1),
            "mcp:plugin:3:1",
            LocalConfigKind::Mcp,
        ),
        (
            ItemId::PluginSkill(0, 40),
            "skill:plugin:0:40",
            LocalConfigKind::Skill,
        ),
    ];
    for (id, text, kind) in ids {
        assert_eq!(id.to_string(), text);
        assert_eq!(ItemId::parse(text), Some(id));
        assert_eq!(id.kind(), kind);
    }
    for bad in [
        "",
        "mcp:user",
        "mcp:user:x",
        "mcp:user:-1",
        "mcp:user:+1",
        "mcp:user:01",
        "mcp:user:1:2",
        "plugin:1:2",
        "skill:plugin:1",
        "mcp:project:1",
        "MCP:user:1",
        "mcp:user:99999999999",
        "mcp:user: 1",
    ] {
        assert_eq!(ItemId::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn error_display_names_source_and_position_only() {
    let e = LocalImportError::Malformed {
        source: SourceRef::PluginMcp(2),
        line: 3,
        column: 7,
    };
    assert_eq!(e.to_string(), "本机配置看不懂：PluginMcp(2) 第 3 行第 7 列");
    let e = LocalImportError::Unreadable {
        source: SourceRef::SkillsDir,
        kind: std::io::ErrorKind::PermissionDenied,
    };
    assert_eq!(
        e.to_string(),
        "本机配置读不了：SkillsDir（PermissionDenied）"
    );
    assert!(!e.retryable());
}

/// 编译期的那一条（RA6）：`DiscoveredName` 与 `LocalImportError` 的字段只有这些类型，没有 `String`、`PathBuf`。
/// 解构不带 `..`：往里加字段（比如一段原文）时这里编译不过，逼着先给它造出口类型。
#[test]
fn exported_types_carry_no_plain_strings() {
    fn discovered(d: DiscoveredName) {
        let DiscoveredName {
            id,
            kind,
            name,
            owner,
            transport,
            value_keys,
            skill_dirs,
        } = d;
        let _: ItemId = id;
        let _: LocalConfigKind = kind;
        let _: Name = name;
        match owner {
            Owner::User => {}
            Owner::Plugin { index, key } => {
                let _: u32 = index;
                let _: Name = key;
            }
        }
        let _: Option<Transport> = transport;
        let _: Vec<Name> = value_keys;
        let _: u32 = skill_dirs;
    }
    fn error(e: LocalImportError) {
        match e {
            LocalImportError::Unreadable { source, kind } => {
                let _: SourceRef = source;
                let _: std::io::ErrorKind = kind;
            }
            LocalImportError::Malformed {
                source,
                line,
                column,
            } => {
                let _: SourceRef = source;
                let _: (usize, usize) = (line, column);
            }
        }
    }
    let _ = (discovered, error);
}
