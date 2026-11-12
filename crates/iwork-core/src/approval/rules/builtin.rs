//! 内置规则集 `m1-1`（T07 方案 4.2；需求第 9 章「安全规则的组织」）。
//!
//! 每条底线、必问、默认值、禁用工具、清单条目都是一条 `RuleEntry`；ID 由固定的 slug 派生
//! （[`builtin_rule_id`]），slug 进 `label`。改任何条目都要升版本号，审计里的版本号才有意义。
//! 条目里的路径按 [`EnvPaths`] 填成绝对路径；ID 与版本不随环境变。
//!
//! 默认层按这里的顺序第一条命中即用（`ToolName` 在前，带标记的在前，裸种类在后）；顺序是数据的
//! 一部分，`builtin_ids_stable` 快照连顺序一起固化。

use std::path::PathBuf;

use iwork_domain::action::{ActionFlag, ActionKind};
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::common::Timestamp;
use iwork_domain::id::{RawUlid, RuleId};
use iwork_domain::rule::{
    RuleEffect, RuleEntry, RuleLayer, RuleMatcher, RuleScope, RuleSetVersion, RuleSource,
};
use sha2::{Digest, Sha256};

use crate::approval::env::{EnvPaths, Platform};
use crate::approval::registry::{self, ToolClass};

/// 内置规则集版本。改任何条目都要升号（m1-2：加 UserDeny 层三条内置拒绝条目，T07 收口；
/// m1-3：永久禁用工具多六个，T06 方案 P13）。
pub const RULESET_VERSION: &str = "m1-3";

pub fn version() -> RuleSetVersion {
    RuleSetVersion(RULESET_VERSION.into())
}

/// 内置条目的稳定 ID：`sha256("iwork.rule." + slug)` 的前 16 字节当 ULID。
pub fn builtin_rule_id(slug: &str) -> RuleId {
    let digest = Sha256::digest(format!("iwork.rule.{slug}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    RuleId::from_raw(RawUlid::from_u128(u128::from_be_bytes(bytes)))
}

use ApprovalMode::{Auto, FullAccess as Full, Manual};
use RuleEffect as E;
use RuleLayer as L;
use RuleMatcher as M;

fn entry(
    slug: impl Into<String>,
    layer: RuleLayer,
    matcher: RuleMatcher,
    effect: RuleEffect,
    modes: &[ApprovalMode],
) -> RuleEntry {
    let slug = slug.into();
    RuleEntry {
        id: builtin_rule_id(&slug),
        layer,
        matcher,
        effect,
        source: RuleSource::Builtin {
            ruleset_version: version(),
        },
        scope: RuleScope::Global,
        created_at: Timestamp(0),
        label: slug,
        applies_in: modes.iter().copied().collect(),
    }
}

const ALL: &[ApprovalMode] = &[];
const MANUAL_AUTO: &[ApprovalMode] = &[Manual, Auto];
const FULL: &[ApprovalMode] = &[Full];

fn kind(k: ActionKind) -> RuleMatcher {
    M::ActionKindIs(k)
}

fn all(ms: impl IntoIterator<Item = RuleMatcher>) -> RuleMatcher {
    M::All(ms.into_iter().collect())
}

fn under(p: PathBuf) -> RuleMatcher {
    M::PathUnder(p)
}

fn is(p: PathBuf) -> RuleMatcher {
    M::PathIs(p)
}

/// 凭据目录与文件（需求第 9 章凭据目录清单）。(slug 尾段, 相对主目录的路径, 是不是目录)
fn credential_paths(platform: Platform) -> Vec<(&'static str, &'static str, bool)> {
    let mut v = vec![
        ("ssh", ".ssh", true),
        ("gnupg", ".gnupg", true),
        ("aws", ".aws", true),
        ("azure", ".azure", true),
        ("kube", ".kube", true),
        ("docker-config", ".docker/config.json", false),
        ("netrc", ".netrc", false),
        ("git-credentials", ".git-credentials", false),
        ("npmrc", ".npmrc", false),
        ("pypirc", ".pypirc", false),
        ("claude-credentials", ".claude/.credentials.json", false),
        ("codex-auth", ".codex/auth.json", false),
    ];
    match platform {
        Platform::MacOs => v.extend([
            ("gcloud", ".config/gcloud", true),
            ("keychains", "Library/Keychains", true),
        ]),
        Platform::Linux => v.extend([
            ("gcloud", ".config/gcloud", true),
            ("keychains", ".local/share/keyrings", true),
        ]),
        Platform::Windows => v.extend([
            ("gcloud", "AppData/Roaming/gcloud", true),
            ("keychains", "AppData/Roaming/Microsoft/Credentials", true),
            (
                "keychains-protect",
                "AppData/Roaming/Microsoft/Protect",
                true,
            ),
        ]),
    }
    v
}

/// 系统目录（删除关键路径；需求第 9 章）。
fn system_dirs(platform: Platform) -> &'static [&'static str] {
    match platform {
        Platform::MacOs => &[
            "/System",
            "/usr",
            "/bin",
            "/sbin",
            "/etc",
            "/Library",
            "/private",
            "/var",
            "/Applications",
            "/opt",
            "/Users",
        ],
        Platform::Linux => &[
            "/usr", "/bin", "/sbin", "/etc", "/lib", "/lib64", "/var", "/opt", "/boot", "/home",
            "/root",
        ],
        Platform::Windows => &[
            r"C:\Windows",
            r"C:\Program Files",
            r"C:\Program Files (x86)",
            r"C:\Users",
        ],
    }
}

fn fs_root(platform: Platform) -> PathBuf {
    match platform {
        Platform::Windows => PathBuf::from(r"C:\"),
        _ => PathBuf::from("/"),
    }
}

/// slug 里用的路径尾段：去掉分隔符与开头的点，`/Program Files (x86)` → `program-files-x86`。
fn slug_of(path: &str) -> String {
    let mut s = String::new();
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.is_empty() && !s.ends_with('-') {
            s.push('-');
        }
    }
    s.trim_end_matches('-').to_owned()
}

/// 内置需凭据命令（Q3：写到子命令一级；整程序只有 gh 与 ssh；scp / sftp / rsync 与云服务 CLI 不内置）。
pub const CREDENTIAL_COMMANDS: &[(&str, Option<&str>)] = &[
    ("git", Some("pull")),
    ("git", Some("fetch")),
    ("git", Some("clone")),
    ("git", Some("push")),
    ("git", Some("ls-remote")),
    ("git", Some("remote")),
    ("git", Some("submodule")),
    ("git", Some("lfs")),
    ("gh", None),
    ("docker", Some("login")),
    ("docker", Some("logout")),
    ("docker", Some("pull")),
    ("docker", Some("push")),
    ("npm", Some("publish")),
    ("npm", Some("login")),
    ("npm", Some("adduser")),
    ("npm", Some("whoami")),
    ("npm", Some("token")),
    ("npm", Some("access")),
    ("npm", Some("owner")),
    ("npm", Some("deprecate")),
    ("npm", Some("unpublish")),
    ("pnpm", Some("publish")),
    ("pnpm", Some("login")),
    ("yarn", Some("publish")),
    ("yarn", Some("login")),
    ("cargo", Some("publish")),
    ("cargo", Some("login")),
    ("cargo", Some("owner")),
    ("cargo", Some("yank")),
    ("twine", Some("upload")),
    ("gem", Some("push")),
    ("gem", Some("signin")),
    ("ssh", None),
];

/// 拒收名单（FR11「解释器、shell 与通用启动器拒收」；Q3 rsync / scp / sftp 不能整程序加入）。
pub const REJECTED_PROGRAMS: &[&str] = &[
    "bash",
    "sh",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "python",
    "python2",
    "python3",
    "node",
    "deno",
    "bun",
    "ruby",
    "perl",
    "php",
    "lua",
    "env",
    "xargs",
    "make",
    "cmake",
    "ninja",
    "sudo",
    "doas",
    "su",
    "nohup",
    "time",
    "nice",
    "exec",
    "eval",
    "command",
    "busybox",
    "osascript",
    "open",
    "nsenter",
    "chroot",
    "docker",
    "rsync",
    "scp",
    "sftp",
];

/// 拒收名单里只拒「整程序」、允许写到子命令一级的程序（`docker login` 可以加，`docker` 不行）。
/// 其余拒收程序带不带第二个词都拒：`python script.py` 的第二个词是脚本，不是子命令（审查 1/F-9）。
pub const REJECT_ALLOWS_SUBCOMMAND: &[&str] = &["docker"];

/// 传输工具：拒收理由与解释器不同（方案 4.4）。
pub const TRANSFER_PROGRAMS: &[&str] = &["rsync", "scp", "sftp"];

/// 会被自动执行的配置（需求第 9 章）。(slug 尾段, 根, 相对路径, 是不是目录)
fn autorun_paths(env: &EnvPaths) -> Vec<(String, PathBuf, bool)> {
    let h = |rel: &str| env.home.0.join(rel);
    let w = |rel: &str| env.workspace_root.0.join(rel);
    let mut v = vec![
        ("claude-settings".to_owned(), w(".claude"), true),
        ("claude-home".to_owned(), h(".claude"), true),
        ("mcp-json".to_owned(), w(".mcp.json"), false),
        ("gitconfig".to_owned(), h(".gitconfig"), false),
        ("git-hooks".to_owned(), w(".git/hooks"), true),
    ];
    for rc in [
        ".zshrc",
        ".zprofile",
        ".zshenv",
        ".bashrc",
        ".bash_profile",
        ".profile",
        ".config/fish/config.fish",
    ] {
        v.push((format!("shell-rc.{}", slug_of(rc)), h(rc), false));
    }
    match env.platform {
        Platform::MacOs => {
            v.push(("launch-agents".into(), h("Library/LaunchAgents"), true));
            v.push((
                "launch-daemons.agents".into(),
                PathBuf::from("/Library/LaunchAgents"),
                true,
            ));
            v.push((
                "launch-daemons.daemons".into(),
                PathBuf::from("/Library/LaunchDaemons"),
                true,
            ));
        }
        Platform::Linux => {
            v.push(("systemd-user".into(), h(".config/systemd/user"), true));
            v.push(("autostart".into(), h(".config/autostart"), true));
        }
        Platform::Windows => {
            v.push((
                "startup".into(),
                h("AppData/Roaming/Microsoft/Windows/Start Menu/Programs/Startup"),
                true,
            ));
        }
    }
    v
}

/// 敏感目录默认清单（需求第 9 章）。
fn sensitive_paths(platform: Platform) -> &'static [(&'static str, &'static str)] {
    match platform {
        Platform::MacOs => &[
            ("documents", "Documents"),
            ("desktop", "Desktop"),
            ("downloads", "Downloads"),
            ("pictures", "Pictures"),
            ("movies", "Movies"),
            ("music", "Music"),
            ("icloud", "Library/Mobile Documents"),
            ("mail", "Library/Mail"),
            ("messages", "Library/Messages"),
            ("chrome", "Library/Application Support/Google/Chrome"),
            ("safari", "Library/Safari"),
            ("firefox", "Library/Application Support/Firefox"),
            ("edge", "Library/Application Support/Microsoft Edge"),
        ],
        Platform::Linux => &[
            ("documents", "Documents"),
            ("desktop", "Desktop"),
            ("downloads", "Downloads"),
            ("pictures", "Pictures"),
            ("videos", "Videos"),
            ("music", "Music"),
            ("thunderbird", ".thunderbird"),
            ("chrome", ".config/google-chrome"),
            ("chromium", ".config/chromium"),
            ("firefox", ".mozilla"),
            ("edge", ".config/microsoft-edge"),
        ],
        Platform::Windows => &[
            ("documents", "Documents"),
            ("desktop", "Desktop"),
            ("downloads", "Downloads"),
            ("pictures", "Pictures"),
            ("videos", "Videos"),
            ("music", "Music"),
            ("chrome", "AppData/Local/Google/Chrome/User Data"),
            ("firefox", "AppData/Roaming/Mozilla/Firefox"),
            ("edge", "AppData/Local/Microsoft/Edge/User Data"),
        ],
    }
}

/// 可写缓存区（需求第 9 章：常见工具链缓存的具体子目录）。
fn cache_paths(platform: Platform) -> Vec<(&'static str, &'static str)> {
    let mut v = vec![
        ("npm", ".npm"),
        ("cargo-registry", ".cargo/registry"),
        ("cargo-git", ".cargo/git"),
        ("go-mod", "go/pkg/mod"),
        ("pip", ".cache/pip"),
        ("gradle", ".gradle"),
        ("m2", ".m2/repository"),
        ("pnpm-store", ".local/share/pnpm/store"),
        ("yarn-cache", ".cache/yarn"),
        ("bun", ".bun/install/cache"),
        ("rustup-toolchains", ".rustup/toolchains"),
        ("cocoapods", ".cocoapods"),
    ];
    if platform == Platform::MacOs {
        v.extend([
            ("pip-macos", "Library/Caches/pip"),
            ("derived-data", "Library/Developer/Xcode/DerivedData"),
            ("cocoapods-cache", "Library/Caches/CocoaPods"),
            ("pnpm-store-macos", "Library/pnpm/store"),
            ("yarn-cache-macos", "Library/Caches/Yarn"),
        ]);
    }
    v
}

/// 内置规则集，路径按 `env` 填好。顺序：底线、工具配置、必问、默认（有序）、四张清单、拒收名单。
pub fn builtin_rules(env: &EnvPaths) -> Vec<RuleEntry> {
    let mut v = Vec::new();
    let home = |rel: &str| env.home.0.join(rel);

    // ① 底线：凭据目录读写都禁
    for (slug, rel, is_dir) in credential_paths(env.platform) {
        let m = if is_dir {
            under(home(rel))
        } else {
            is(home(rel))
        };
        v.push(entry(
            format!("baseline.cred.{slug}"),
            L::Baseline,
            m,
            E::UserActionEligible,
            ALL,
        ));
    }
    // 数据目录读写都禁，只读区只放行读（v1.64、v1.68）
    v.push(entry(
        "baseline.data-dir",
        L::Baseline,
        under(env.data_dir.0.clone()),
        E::UserActionEligible,
        ALL,
    ));
    for zone in ["attachments", "imports"] {
        v.push(entry(
            format!("baseline.data-dir.readonly.{zone}"),
            L::Baseline,
            under(env.data_dir.0.join(zone)),
            E::Allow,
            ALL,
        ));
    }
    // 写入 .git 内部（只拦直接写文件；git 命令的目标是命令，不命中路径匹配）
    let git_dir = env.workspace_root.0.join(".git");
    for (slug, k) in [
        ("write", ActionKind::Write),
        ("delete", ActionKind::Delete),
        ("autorun", ActionKind::WriteAutorunConfig),
    ] {
        v.push(entry(
            format!("baseline.git-internal.{slug}"),
            L::Baseline,
            all([under(git_dir.clone()), kind(k)]),
            E::UserActionEligible,
            ALL,
        ));
    }
    // 删除关键路径：底线层的 PathIs 对删除动作还匹配「目标是它的上级」
    let mut critical: Vec<(String, PathBuf)> = vec![
        ("root".into(), fs_root(env.platform)),
        ("home".into(), env.home.0.clone()),
        ("workspace".into(), env.workspace_root.0.clone()),
        ("workspace-git".into(), git_dir.clone()),
        ("data-dir".into(), env.data_dir.0.clone()),
        ("iwork-root".into(), env.iwork_root.0.clone()),
        ("iwork-projects".into(), env.iwork_root.0.join("projects")),
        (
            "iwork-assistants".into(),
            env.iwork_root.0.join("assistants"),
        ),
    ];
    for dir in system_dirs(env.platform) {
        critical.push((format!("system.{}", slug_of(dir)), PathBuf::from(dir)));
    }
    for (slug, p) in critical {
        v.push(entry(
            format!("baseline.critical.{slug}"),
            L::Baseline,
            all([kind(ActionKind::Delete), is(p)]),
            E::UserActionEligible,
            ALL,
        ));
    }

    // ③ 工具配置：能把工作带出 iWork 管控的工具三档拒绝
    for name in registry::tools_of(|t| t.class == ToolClass::Disabled) {
        v.push(entry(
            format!("toolconfig.disabled.{name}"),
            L::ToolConfig,
            M::ToolName(name.into()),
            E::Deny,
            ALL,
        ));
    }

    // ④ 拒绝：命令或路径检查不完整时三档拒绝、不出卡片（方案 3.3、3.5、4.2）。和用户拒绝规则在同一层，
    // 求值第 4 步原样产出 Deny(DenyRule(id))，排在必问与放行规则之前，求值流程不用改
    // label 是回给 Agent 的「被规则拒绝：…」后半句
    for (slug, flag, label) in [
        (
            "deny.resource-limit",
            ActionFlag::ResourceLimitHit,
            "命令太大或展开出的检查项过多，请拆成几条重发",
        ),
        (
            "deny.unlexable",
            ActionFlag::Unlexable,
            "命令有语法错误，无法检查，请修正后重发",
        ),
        (
            "deny.path-unresolved",
            ActionFlag::PathUnresolved,
            "路径查不清（权限不足等），无法检查；请先解决再重发",
        ),
    ] {
        let mut e = entry(slug, L::UserDeny, M::Flag(flag), E::Deny, ALL);
        e.label = label.into();
        v.push(e);
    }

    // ⑤ 必问
    v.push(entry(
        "mustask.push",
        L::MustAsk,
        kind(ActionKind::Push),
        E::Ask,
        ALL,
    ));
    for (slug, k) in [
        ("publish", ActionKind::Publish),
        ("autorun-config", ActionKind::WriteAutorunConfig),
        ("delete", ActionKind::Delete),
    ] {
        v.push(entry(
            format!("mustask.{slug}"),
            L::MustAsk,
            kind(k),
            E::Ask,
            MANUAL_AUTO,
        ));
    }

    // ⑦ 默认层（有序）
    for name in registry::tools_of(|t| t.class == ToolClass::Harmless) {
        v.push(entry(
            format!("default.harmless.{name}"),
            L::Default,
            M::ToolName(name.into()),
            E::Allow,
            ALL,
        ));
    }
    let oob = || M::Flag(ActionFlag::OutOfBounds);
    let defaults: Vec<(&str, RuleMatcher, RuleEffect, &[ApprovalMode])> = vec![
        (
            "default.read.oob",
            all([kind(ActionKind::Read), oob()]),
            E::Ask,
            MANUAL_AUTO,
        ),
        (
            "default.read.oob.full",
            all([kind(ActionKind::Read), oob()]),
            E::Allow,
            FULL,
        ),
        ("default.read", kind(ActionKind::Read), E::Allow, ALL),
        (
            "default.write.oob",
            all([kind(ActionKind::Write), oob()]),
            E::Ask,
            MANUAL_AUTO,
        ),
        (
            "default.write.oob.full",
            all([kind(ActionKind::Write), oob()]),
            E::Allow,
            FULL,
        ),
        // 角色未知的参数路径在边界内不单独再问：执行动作本身仍按档位问（supervisor 2026-11-10 定 (a)）
        (
            "default.write.argument",
            all([kind(ActionKind::Write), M::Flag(ActionFlag::ArgumentPath)]),
            E::Allow,
            ALL,
        ),
        (
            "default.write.manual",
            kind(ActionKind::Write),
            E::Ask,
            &[Manual],
        ),
        (
            "default.write.auto-full",
            kind(ActionKind::Write),
            E::Allow,
            &[Auto, Full],
        ),
        (
            "default.execute.manual",
            kind(ActionKind::Execute),
            E::Ask,
            &[Manual],
        ),
        (
            "default.execute.auto-full",
            kind(ActionKind::Execute),
            E::Allow,
            &[Auto, Full],
        ),
        (
            "default.outside.retry.manual-auto",
            all([
                kind(ActionKind::OutsideSandbox),
                M::Flag(ActionFlag::BypassSandboxRequested),
            ]),
            E::OutsideSandbox,
            MANUAL_AUTO,
        ),
        (
            "default.outside.retry.full",
            all([
                kind(ActionKind::OutsideSandbox),
                M::Flag(ActionFlag::BypassSandboxRequested),
            ]),
            E::Deny,
            FULL,
        ),
        (
            "default.outside.listed.manual-auto",
            kind(ActionKind::OutsideSandbox),
            E::OutsideSandbox,
            MANUAL_AUTO,
        ),
        (
            "default.outside.listed.full",
            kind(ActionKind::OutsideSandbox),
            E::Allow,
            FULL,
        ),
        // 手动、自动档下这三种已被必问层接走；这里只为默认层对每种动作都有结论（default_layer_total）
        (
            "default.autorun-config.manual-auto",
            kind(ActionKind::WriteAutorunConfig),
            E::Ask,
            MANUAL_AUTO,
        ),
        (
            "default.autorun-config.full",
            kind(ActionKind::WriteAutorunConfig),
            E::Allow,
            FULL,
        ),
        (
            "default.delete.manual-auto",
            kind(ActionKind::Delete),
            E::Ask,
            MANUAL_AUTO,
        ),
        (
            "default.delete.full",
            kind(ActionKind::Delete),
            E::Allow,
            FULL,
        ),
        (
            "default.publish.manual-auto",
            kind(ActionKind::Publish),
            E::Ask,
            MANUAL_AUTO,
        ),
        (
            "default.publish.full",
            kind(ActionKind::Publish),
            E::Allow,
            FULL,
        ),
        // 兜底：正常永远被 mustask.push 先接走
        ("default.push", kind(ActionKind::Push), E::Ask, ALL),
        ("default.network", kind(ActionKind::Network), E::Allow, ALL),
        (
            "default.mcp.manual-auto",
            kind(ActionKind::McpCall),
            E::Ask,
            MANUAL_AUTO,
        ),
        (
            "default.mcp.full",
            kind(ActionKind::McpCall),
            E::Allow,
            FULL,
        ),
        (
            "default.unclassified.manual-auto",
            kind(ActionKind::Unclassified),
            E::Ask,
            MANUAL_AUTO,
        ),
        (
            "default.unclassified.full",
            kind(ActionKind::Unclassified),
            E::Allow,
            FULL,
        ),
    ];
    for (slug, m, effect, modes) in defaults {
        v.push(entry(slug, L::Default, m, effect, modes));
    }

    // 清单：需凭据命令
    for (program, sub) in CREDENTIAL_COMMANDS {
        let slug = match sub {
            Some(s) => format!("credential.{program}.{s}"),
            None => format!("credential.{program}"),
        };
        v.push(entry(
            slug,
            L::CredentialCommands,
            M::ProgramSubcommand {
                program: (*program).into(),
                subcommand: sub.map(Into::into),
            },
            E::OutsideSandbox,
            ALL,
        ));
    }
    // 会被自动执行的配置
    for (slug, p, is_dir) in autorun_paths(env) {
        let m = if is_dir { under(p) } else { is(p) };
        v.push(entry(
            format!("autorun.{slug}"),
            L::AutorunConfig,
            m,
            E::Ask,
            ALL,
        ));
    }
    // 敏感目录
    for (slug, rel) in sensitive_paths(env.platform) {
        v.push(entry(
            format!("sensitive.{slug}"),
            L::SensitiveDirs,
            under(home(rel)),
            E::Ask,
            ALL,
        ));
    }
    // 可写缓存
    for (slug, rel) in cache_paths(env.platform) {
        v.push(entry(
            format!("cache.{slug}"),
            L::WritableCache,
            under(home(rel)),
            E::Allow,
            ALL,
        ));
    }
    // 拒收名单
    for program in REJECTED_PROGRAMS {
        v.push(entry(
            format!("reject.{program}"),
            L::RejectList,
            M::ProgramSubcommand {
                program: (*program).into(),
                subcommand: None,
            },
            E::Deny,
            ALL,
        ));
    }
    v
}

/// iWork 登记的其他项目目录与其他助手的私聊目录：会话快照里的临时敏感条目，不入库，ID 由路径派生。
pub fn transient_sensitive_rules(env: &EnvPaths) -> Vec<RuleEntry> {
    env.other_project_roots
        .iter()
        .map(|p| ("other-project", p))
        .chain(env.other_dm_dirs.iter().map(|p| ("other-dm", p)))
        .map(|(kind, p)| {
            entry(
                format!("sensitive.{kind}.{}", p.0.display()),
                L::SensitiveDirs,
                under(p.0.clone()),
                E::Ask,
                ALL,
            )
        })
        .collect()
}
