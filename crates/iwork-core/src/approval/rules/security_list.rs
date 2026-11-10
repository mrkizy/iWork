//! 安全清单用户条目的校验（T07 方案 4.4；AC41）。纯函数：路径只做词法规范化，不碰磁盘
//! （`PathResolver` 只用来按文件系统的大小写规则比较）；求值时再解析符号链接。

use std::path::{Component, Path, PathBuf};

use iwork_domain::action::CanonicalPath;
use iwork_domain::common::Timestamp;
use iwork_domain::id::RuleId;
use iwork_domain::rule::{
    RuleEffect, RuleEntry, RuleLayer, RuleMatcher, RuleScope, RuleSet, RuleSource,
};
use iwork_domain::security_list::{ListEntryRejection, SecurityList};
use iwork_ports::fs::PathResolver;

use super::builtin::{REJECT_ALLOWS_SUBCOMMAND, TRANSFER_PROGRAMS};
use super::matcher::path_of;
use crate::approval::env::EnvPaths;

/// 校验要用的上下文。
pub struct ListValidationContext<'a> {
    pub paths: &'a EnvPaths,
    /// 范围为某项目时只有那一个；全局时全部已登记项目
    pub project_roots: &'a [CanonicalPath],
    /// 取拒收名单、底线路径、自动执行配置、可写缓存、已有条目（含内置）
    pub rules: &'a RuleSet,
}

/// 用户新增一条清单条目：通过返回要持久化的规则条目，拒收返回给人看的理由。
pub fn validate_list_entry(
    list: SecurityList,
    value: &str,
    scope: RuleScope,
    ctx: &ListValidationContext,
    fs: &impl PathResolver,
    now: Timestamp,
    id: RuleId,
) -> Result<RuleEntry, ListEntryRejection> {
    let value = value.trim();
    let (matcher, effect) = match list {
        SecurityList::CredentialCommands => {
            (credential_command(value, ctx)?, RuleEffect::OutsideSandbox)
        }
        SecurityList::SensitiveDirs => (
            RuleMatcher::PathUnder(sensitive_dir(value, ctx, fs)?.0),
            RuleEffect::Ask,
        ),
        SecurityList::WritableDirs => (
            RuleMatcher::PathUnder(writable_dir(value, ctx, fs)?.0),
            RuleEffect::Allow,
        ),
    };
    let layer = list.layer();
    let duplicate = ctx.rules.entries.iter().any(|e| {
        e.layer == layer
            && e.matcher == matcher
            && (e.scope == scope || e.scope == RuleScope::Global)
    });
    if duplicate {
        return Err(reject(format!("「{value}」已在清单里")));
    }
    Ok(RuleEntry {
        id,
        layer,
        matcher,
        effect,
        source: RuleSource::User,
        scope,
        created_at: now,
        label: value.to_owned(),
        applies_in: Default::default(),
    })
}

fn reject(reason: impl Into<String>) -> ListEntryRejection {
    ListEntryRejection {
        reason: reason.into(),
    }
}

fn is_plain_word(w: &str) -> bool {
    !w.is_empty()
        && !w.starts_with('-')
        && !w.contains(['/', '\\', '=', '*', '?', '[', '$', '`', '\'', '"', '~'])
}

fn credential_command(
    value: &str,
    ctx: &ListValidationContext,
) -> Result<RuleMatcher, ListEntryRejection> {
    let words: Vec<&str> = value.split_whitespace().collect();
    let (program, subcommand) = match words.as_slice() {
        [p] => (*p, None),
        [p, s] => (*p, Some(*s)),
        [] => return Err(reject("命令不能为空")),
        _ => return Err(reject("只能写程序名加一个子命令")),
    };
    if program.contains(['/', '\\']) {
        return Err(reject(format!("「{program}」不能写路径，只写程序名")));
    }
    if !is_plain_word(program) {
        return Err(reject(format!("「{program}」不是纯程序名")));
    }
    if let Some(s) = subcommand
        && !is_plain_word(s)
    {
        return Err(reject(format!(
            "「{s}」不是子命令（不能以 - 开头、不能含通配或路径）"
        )));
    }
    let rejected = ctx.rules.entries.iter().any(|e| {
        e.layer == RuleLayer::RejectList
            && matches!(&e.matcher, RuleMatcher::ProgramSubcommand { program: p, subcommand: None } if p == program)
    });
    // 拒收程序带不带子命令都拒；只有登记为「允许子命令」的（docker）才放过带子命令的写法
    let subcommand_allowed = subcommand.is_some() && REJECT_ALLOWS_SUBCOMMAND.contains(&program);
    if rejected && !subcommand_allowed {
        return Err(reject(if TRANSFER_PROGRAMS.contains(&program) {
            format!("「{program}」会把任意文件传出去，不能整程序加入；请改用 git，或由你亲手执行")
        } else {
            format!(
                "「{program}」是解释器 / 启动器，清单内的命令会在沙箱外运行，等于放开整个沙箱"
            )
        }));
    }
    Ok(RuleMatcher::ProgramSubcommand {
        program: program.to_owned(),
        subcommand: subcommand.map(str::to_owned),
    })
}

/// 绝对路径或 `~/` 开头；词法规范化（去 `.`、`..`，不解析符号链接）。
fn lexical_path(value: &str, env: &EnvPaths) -> Result<CanonicalPath, ListEntryRejection> {
    let raw = if value == "~" {
        env.home.0.clone()
    } else if let Some(rest) = value.strip_prefix("~/") {
        env.home.0.join(rest)
    } else if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        return Err(reject(format!("「{value}」要写绝对路径或以 ~/ 开头")));
    };
    let mut out = PathBuf::new();
    for c in raw.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Ok(CanonicalPath(out))
}

/// 两条路径有包含关系（任一在另一之下）。
fn overlaps(fs: &impl PathResolver, a: &CanonicalPath, b: &CanonicalPath) -> bool {
    fs.is_under(a, b) || fs.is_under(b, a)
}

fn is_root(p: &CanonicalPath) -> bool {
    p.0.parent().is_none()
}

fn sensitive_dir(
    value: &str,
    ctx: &ListValidationContext,
    fs: &impl PathResolver,
) -> Result<CanonicalPath, ListEntryRejection> {
    let p = lexical_path(value, ctx.paths)?;
    if is_root(&p) || fs.same_path(&p, &ctx.paths.home) {
        return Err(reject(format!("「{value}」范围太大")));
    }
    if let Some(root) = ctx.project_roots.iter().find(|r| fs.is_under(r, &p)) {
        return Err(reject(format!(
            "「{value}」包含项目工作区 {}，成员会读不了自己的代码",
            root.0.display()
        )));
    }
    Ok(p)
}

fn layer_paths(
    rules: &RuleSet,
    layer: RuleLayer,
) -> impl Iterator<Item = (&RuleEntry, CanonicalPath)> {
    rules
        .entries
        .iter()
        .filter(move |e| e.layer == layer)
        .filter_map(|e| path_of(&e.matcher).map(|p| (e, CanonicalPath(p.to_path_buf()))))
}

fn writable_dir(
    value: &str,
    ctx: &ListValidationContext,
    fs: &impl PathResolver,
) -> Result<CanonicalPath, ListEntryRejection> {
    let p = lexical_path(value, ctx.paths)?;
    if is_root(&p) || fs.same_path(&p, &ctx.paths.home) {
        return Err(reject(format!("「{value}」范围太大")));
    }
    // ① 底线路径（凭据、数据目录、.git 内部）之下或是它们的上级；
    // ② 关键路径本身，或系统目录之下（不含主目录所在的 /Users、/home）
    for (e, bp) in layer_paths(ctx.rules, RuleLayer::Baseline) {
        let hit = if e.label.starts_with("baseline.critical.") {
            let system_dir = e.label.starts_with("baseline.critical.system.")
                && !fs.is_under(&ctx.paths.home, &bp);
            fs.same_path(&p, &bp) || (system_dir && fs.is_under(&p, &bp))
        } else {
            overlaps(fs, &p, &bp)
        };
        if hit {
            return Err(reject(format!(
                "「{value}」与受保护的路径 {} 重叠（{}）",
                bp.0.display(),
                e.label
            )));
        }
    }
    // ③ 会被自动执行的配置之下或是它的上级
    for (e, ap) in layer_paths(ctx.rules, RuleLayer::AutorunConfig) {
        if overlaps(fs, &p, &ap) {
            return Err(reject(format!(
                "「{value}」包含 {}（{}），写进去的东西会在 iWork 之外自动运行",
                ap.0.display(),
                e.label
            )));
        }
    }
    // ④ 已经可写
    let already = ctx
        .project_roots
        .iter()
        .cloned()
        .chain(layer_paths(ctx.rules, RuleLayer::WritableCache).map(|(_, c)| c))
        .chain(ctx.paths.tmp_dirs.iter().cloned())
        .find(|w| fs.is_under(&p, w));
    if let Some(w) = already {
        return Err(reject(format!(
            "「{value}」在 {} 之下，已经可写，不用加",
            w.0.display()
        )));
    }
    Ok(p)
}
