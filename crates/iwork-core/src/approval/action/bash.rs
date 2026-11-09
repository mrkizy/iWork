//! Bash 调用 → 待解析的动作（T07 方案 3.4–3.6）。纯函数，不碰文件系统：路径词原样留着，
//! 由 `PathResolver` 取来事实后在 `normalize` 里换成规范路径。
//!
//! 归类顺序（按顺序查的登记表）：① 整条解析不了的触发 → ② `git` 子命令 → ③ 发布类 → ④ 白名单
//! 参数规则全部满足 → 读 / 写 / 删除 → ⑤ 其余归执行。②–⑤ 的路径动作按登记的参数角色精确提取；
//! 解析不了的调用另走 [`fallback_actions`]，不认结构、宁可多抽。

use iwork_domain::action::ActionKind;
use iwork_shell::{PartialFacts, RedirectKind, ShellScript, SimpleCommand};

use super::commands::{
    self, ArgSpec, DD_KEYS, FIND_FLAG_PREDICATES, FIND_LEADING_FLAGS, FIND_PATH_PREDICATES,
    FIND_VALUE_PREDICATES, GIT_GLOBAL_FLAGS, GIT_RELOCATING, INLINE_CODE_OPTIONS, OPAQUE_PROGRAMS,
    OPAQUE_SUBCOMMANDS, Operands, PUBLISH_COMMANDS, Role, SHELLS, UNPARSABLE_PROGRAMS, Value,
};
use super::credential::simple_commands;
use super::paths::{fallback_candidates, is_path_word, split_assignment};
use super::{PendingAction, PendingTarget};

/// 这些程序的未登记短选项粘值不让整条解析不了，改为按兜底候选抽读 + 写：`git` 的种类由子命令决定
/// （推送、丢弃改动按种类必问），`git clean -fd`、`git log -n5` 这类组合短选项是日常写法（方案 11.3 验收 4b
/// 要求 `git clean -fd` 是必问而不是解析不了）。候选里任何一个解析到受保护路径仍是底线拒绝。
const GLUED_AS_CANDIDATES: &[&str] = &["git"];

/// 一个子命令「不是安全形态」：归执行，路径按表外规则抽。
struct NotSafe;

/// 整条解析不了的原因。
pub type Why = String;

/// 已解析的脚本 → 动作；任一子命令触发整条解析不了时返回 `Err(原因)`。
pub fn classify_script(script: &ShellScript) -> Result<Vec<PendingAction>, Why> {
    let commands = simple_commands(script);
    if commands.is_empty() {
        return Err("空命令".into());
    }
    let mut out = Vec::new();
    for c in commands {
        classify_simple(c, &mut out)?;
    }
    Ok(out)
}

fn path(kind: ActionKind, word: &str) -> PendingAction {
    PendingAction {
        kind,
        target: PendingTarget::Path(word.to_owned()),
        opaque: false,
        argument: false,
    }
}

fn role_kind(role: Role) -> ActionKind {
    match role {
        Role::Read => ActionKind::Read,
        Role::Write => ActionKind::Write,
        Role::Delete => ActionKind::Delete,
    }
}

fn push_roles(out: &mut Vec<PendingAction>, roles: &[Role], word: &str) {
    if is_path_word(word) {
        out.extend(roles.iter().map(|r| path(role_kind(*r), word)));
    }
}

/// 角色未知的参数路径：既记读又记写（不知道它会读还是写，两种都要过底线与越界）。
fn push_argument(out: &mut Vec<PendingAction>, word: &str) {
    if is_path_word(word) {
        for kind in [ActionKind::Read, ActionKind::Write] {
            out.push(PendingAction {
                argument: true,
                ..path(kind, word)
            });
        }
    }
}

fn redirect_actions(c: &SimpleCommand, out: &mut Vec<PendingAction>) {
    for r in &c.redirects {
        push_redirect(out, r.kind, &r.target.literal);
    }
}

fn push_redirect(out: &mut Vec<PendingAction>, kind: RedirectKind, target: &str) {
    if kind.reads() {
        push_roles(out, &[Role::Read], target);
    }
    if kind.writes() {
        push_roles(out, &[Role::Write], target);
    }
}

fn classify_simple(c: &SimpleCommand, out: &mut Vec<PendingAction>) -> Result<(), Why> {
    redirect_actions(c, out);
    let words: Vec<&str> = c.words.iter().map(|w| w.literal.as_str()).collect();
    let Some(&program) = words.first() else {
        return Ok(());
    };
    let args = &words[1..];

    // ① 整条解析不了
    if UNPARSABLE_PROGRAMS.contains(&program) {
        return Err(format!("{program} 改变执行方式或工作目录"));
    }
    if SHELLS.contains(&program)
        && args.iter().any(|a| {
            *a == "--command" || (a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
        })
    {
        return Err(format!("{program} -c：程序文本来自参数"));
    }
    if let Some((_, opts)) = INLINE_CODE_OPTIONS.iter().find(|(p, _)| *p == program)
        && args.iter().any(|a| {
            opts.iter()
                .any(|o| a == o || (o.len() == 2 && a.starts_with(o)))
        })
    {
        return Err(format!("{program}：程序文本来自参数"));
    }

    // ② git
    if program == "git" {
        return classify_git(&words, out);
    }
    // ③ 发布类
    if let Some((start, seq)) = publish_match(program, args) {
        let consumed = start..start + seq.len();
        out.push(command_action(
            ActionKind::Publish,
            &words,
            Some(seq[0]),
            false,
        ));
        let rest: Vec<&str> = args
            .iter()
            .enumerate()
            .filter(|(i, _)| !consumed.contains(i))
            .map(|(_, a)| *a)
            .collect();
        return extract_external(program, &rest, out, false);
    }
    // ④ 白名单
    let safe = match program {
        "find" => find_form(args),
        "dd" => dd_form(args),
        _ => match commands::safe_command(program) {
            Some(spec) => safe_form(spec, args),
            None => Err(NotSafe),
        },
    };
    match safe {
        Ok(actions) => {
            out.extend(actions);
            Ok(())
        }
        // ⑤ 执行
        Err(NotSafe) => {
            let sub = args.first().copied().filter(|a| !a.starts_with('-'));
            out.push(command_action(
                ActionKind::Execute,
                &words,
                sub,
                is_opaque(program, sub),
            ));
            // find 的谓词是单横线长词（-delete、-exec），不是短选项粘值
            extract_external(program, args, out, program == "find")
        }
    }
}

fn command_action(
    kind: ActionKind,
    words: &[&str],
    subcommand: Option<&str>,
    opaque: bool,
) -> PendingAction {
    PendingAction {
        kind,
        target: PendingTarget::Command {
            program: words[0].to_owned(),
            subcommand: subcommand.map(str::to_owned),
            argv: words[1..].iter().map(|w| (*w).to_owned()).collect(),
        },
        opaque,
        argument: false,
    }
}

fn is_opaque(program: &str, sub: Option<&str>) -> bool {
    program.contains('/')
        || OPAQUE_PROGRAMS.contains(&program)
        || sub.is_some_and(|s| OPAQUE_SUBCOMMANDS.contains(&(program, s)))
}

fn publish_match(program: &str, args: &[&str]) -> Option<(usize, &'static [&'static str])> {
    PUBLISH_COMMANDS
        .iter()
        .filter(|(p, _)| *p == program)
        .find_map(|(_, seq)| {
            (0..args.len())
                .find(|&i| args[i..].starts_with(seq))
                .map(|i| (i, *seq))
        })
}

/// 表外程序的参数规则（方案 3.4）：位置参数、`--` 之后、`name=value` 的值、`--opt=VALUE` 的值记读 +
/// 写；登记过的路径选项按角色；没登记的短选项带粘值整条解析不了。
fn extract_external(
    program: &str,
    args: &[&str],
    out: &mut Vec<PendingAction>,
    single_dash_long: bool,
) -> Result<(), Why> {
    let registered = commands::path_options(program);
    let mut after_dashdash = false;
    let mut i = 0;
    while i < args.len() {
        let w = args[i];
        if after_dashdash || !w.starts_with('-') || w == "-" {
            push_argument(out, w);
            if !after_dashdash && let Some(v) = split_assignment(w) {
                push_argument(out, v);
            }
        } else if w == "--" {
            after_dashdash = true;
        } else if let Some(long) = w.strip_prefix("--") {
            if let Some((_, v)) = long.split_once('=')
                && !v.is_empty()
            {
                push_argument(out, v);
            }
        } else {
            let letters = &w[1..];
            let first = letters.chars().next().expect("w 以 - 开头且不是单独的 -");
            if let Some((_, roles)) = registered.iter().find(|(c, _)| *c == first) {
                let glued = &letters[first.len_utf8()..];
                if !glued.is_empty() {
                    push_roles(out, roles, glued);
                } else if let Some(v) = args.get(i + 1) {
                    push_roles(out, roles, v);
                    i += 1;
                }
            } else if single_dash_long {
                push_argument(out, w);
            } else if letters.chars().count() >= 2 {
                if !GLUED_AS_CANDIDATES.contains(&program) {
                    return Err(format!("{program} 的选项 {w} 粘着值（未登记的短选项）"));
                }
                // 组合短选项或粘值分不清：每个切分位置之后的剩余部分都按角色未知的路径抽（与兜底同一规则）
                for cand in fallback_candidates(w) {
                    push_argument(out, &cand);
                }
            }
        }
        i += 1;
    }
    Ok(())
}

fn classify_git(words: &[&str], out: &mut Vec<PendingAction>) -> Result<(), Why> {
    let mut i = 1;
    while let Some(&w) = words.get(i) {
        if !w.starts_with('-') {
            break;
        }
        if GIT_RELOCATING
            .iter()
            .any(|r| w == *r || w.starts_with(&format!("{r}=")))
        {
            return Err(format!("git {w} 改变仓库位置"));
        }
        if w == "-c" {
            // 配置值可能指向路径（core.hooksPath=.git/hooks）：按参数规则抽
            if let Some(v) = words.get(i + 1) {
                extract_external("git", &[v], out, false)?;
            }
            i += 2;
        } else if let Some(v) = w.strip_prefix("-c") {
            extract_external("git", &[v], out, false)?;
            i += 1;
        } else if GIT_GLOBAL_FLAGS.contains(&w) {
            i += 1;
        } else {
            return Err(format!("git 子命令前有未登记的选项 {w}"));
        }
    }
    let sub = words.get(i).copied();
    let rest: &[&str] = words.get(i + 1..).unwrap_or(&[]);
    let has = |opts: &[&str]| rest.iter().any(|a| opts.contains(a));
    let kind = match sub {
        Some("push") => ActionKind::Push,
        Some("clean" | "rebase") => ActionKind::Delete,
        Some("reset") if has(&["--hard", "--merge", "--keep"]) => ActionKind::Delete,
        Some("checkout") if has(&["--", "."]) => ActionKind::Delete,
        Some("restore") if !(has(&["--staged", "-S"]) && !has(&["--worktree", "-W"])) => {
            ActionKind::Delete
        }
        Some("stash") if matches!(rest.first(), Some(&"drop" | &"clear")) => ActionKind::Delete,
        Some("branch") if rest.iter().any(|a| is_branch_delete(a)) => ActionKind::Delete,
        Some("worktree") if rest.first() == Some(&"remove") => ActionKind::Delete,
        _ => ActionKind::Execute,
    };
    if kind == ActionKind::Push {
        let remote = rest
            .iter()
            .find(|a| !a.starts_with('-'))
            .copied()
            .unwrap_or("origin");
        out.push(PendingAction {
            kind,
            target: PendingTarget::Remote(remote.to_owned()),
            opaque: false,
            argument: false,
        });
    } else {
        // 丢弃改动的 git 命令目标是命令，不是路径：必问层按种类命中，不会撞上「删除工作区」底线（F-13）
        out.push(command_action(kind, words, sub, false));
    }
    extract_external("git", rest, out, false)
}

/// `-d`、`-D`、`--delete`，以及含 d / D 的组合短选项（`-Df`）。
fn is_branch_delete(a: &str) -> bool {
    a == "--delete" || (a.starts_with('-') && !a.starts_with("--") && a[1..].contains(['d', 'D']))
}

/// 白名单程序：全部选项登记过、位置参数个数与角色对得上，才是安全形态。
fn safe_form(spec: &ArgSpec, args: &[&str]) -> Result<Vec<PendingAction>, NotSafe> {
    let mut out = Vec::new();
    let mut operands: Vec<&str> = Vec::new();
    let mut target_dir = false;
    let mut pattern_given = false;
    let mut all_write = false;
    let mut after_dashdash = false;
    let mut apply = |value: Value, v: &str, out: &mut Vec<PendingAction>| match value {
        Value::Word => {}
        Value::Path(roles) => push_roles(out, roles, v),
        Value::TargetDir => {
            target_dir = true;
            push_roles(out, &[Role::Write], v);
        }
        Value::PatternWord => pattern_given = true,
        Value::PatternFile => {
            pattern_given = true;
            push_roles(out, &[Role::Read], v);
        }
        Value::AllWriteFlag => unreachable!("不取值的选项不走这里"),
    };
    let mut i = 0;
    while i < args.len() {
        let w = args[i];
        if after_dashdash || !w.starts_with('-') || w == "-" {
            if w == "-" && !after_dashdash {
                // 标准输入：不是路径
            } else {
                operands.push(w);
            }
        } else if w == "--" {
            after_dashdash = true;
        } else if let Some(long) = w.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            if inline.is_none() && spec.long_flags.contains(&name) {
                // 不取值
            } else if let Some((_, value)) = spec.long_values.iter().find(|(n, _)| *n == name) {
                let v = match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        *args.get(i).ok_or(NotSafe)?
                    }
                };
                apply(*value, v, &mut out);
            } else {
                return Err(NotSafe);
            }
        } else {
            let letters: Vec<char> = w[1..].chars().collect();
            let mut j = 0;
            while j < letters.len() {
                let c = letters[j];
                if spec.flags.contains(c) {
                    j += 1;
                    continue;
                }
                let Some((_, value)) = spec.short_values.iter().find(|(s, _)| *s == c) else {
                    return Err(NotSafe);
                };
                if *value == Value::AllWriteFlag {
                    all_write = true;
                    j += 1;
                    continue;
                }
                let glued: String = letters[j + 1..].iter().collect();
                let v = if glued.is_empty() {
                    i += 1;
                    (*args.get(i).ok_or(NotSafe)?).to_owned()
                } else {
                    glued
                };
                apply(*value, &v, &mut out);
                break;
            }
        }
        i += 1;
    }
    if spec.max_operands.is_some_and(|m| operands.len() > m) {
        return Err(NotSafe);
    }
    let primary = if all_write {
        for o in &operands {
            push_roles(&mut out, &[Role::Write], o);
        }
        Role::Write
    } else {
        match spec.operands {
            Operands::All(roles) => {
                for o in &operands {
                    push_roles(&mut out, roles, o);
                }
                roles.first().copied().unwrap_or(Role::Read)
            }
            Operands::After { skip, roles } => {
                let skip = if pattern_given { 0 } else { skip };
                if operands.len() < skip {
                    return Err(NotSafe);
                }
                for o in &operands[skip..] {
                    push_roles(&mut out, roles, o);
                }
                roles[0]
            }
            Operands::SourcesThenDest { source, dest } => {
                let (sources, dests) = if target_dir {
                    (&operands[..], &[][..])
                } else {
                    if operands.len() < 2 {
                        return Err(NotSafe);
                    }
                    operands.split_at(operands.len() - 1)
                };
                if sources.is_empty() {
                    return Err(NotSafe);
                }
                for o in sources {
                    push_roles(&mut out, source, o);
                }
                for o in dests {
                    push_roles(&mut out, dest, o);
                }
                dest[0]
            }
        }
    };
    if out.is_empty() {
        // 读标准输入等：没有路径目标，仍记一个动作，审计看得到
        out.push(PendingAction {
            kind: role_kind(primary),
            target: PendingTarget::None,
            opaque: false,
            argument: false,
        });
    }
    Ok(out)
}

/// `find`：起点目录记读；表达式词只认纯测试谓词，其余（`-delete`、`-exec`…）不是安全形态。
fn find_form(args: &[&str]) -> Result<Vec<PendingAction>, NotSafe> {
    let mut out = Vec::new();
    let mut i = 0;
    while args.get(i).is_some_and(|a| FIND_LEADING_FLAGS.contains(a)) {
        i += 1;
    }
    let mut starts = 0;
    while let Some(&a) = args.get(i) {
        if a.starts_with('-') || a == "(" || a == "!" {
            break;
        }
        push_roles(&mut out, &[Role::Read], a);
        starts += 1;
        i += 1;
    }
    if starts == 0 {
        push_roles(&mut out, &[Role::Read], ".");
    }
    while let Some(&a) = args.get(i) {
        if FIND_FLAG_PREDICATES.contains(&a) {
            i += 1;
        } else if FIND_VALUE_PREDICATES.contains(&a) {
            args.get(i + 1).ok_or(NotSafe)?;
            i += 2;
        } else if FIND_PATH_PREDICATES.contains(&a) {
            push_roles(&mut out, &[Role::Read], args.get(i + 1).ok_or(NotSafe)?);
            i += 2;
        } else {
            return Err(NotSafe);
        }
    }
    Ok(out)
}

/// `dd`：只认登记的 `key=value`，且要有 `of=`。
fn dd_form(args: &[&str]) -> Result<Vec<PendingAction>, NotSafe> {
    let mut out = Vec::new();
    let mut has_of = false;
    for a in args {
        let (k, v) = a.split_once('=').ok_or(NotSafe)?;
        let (_, role) = DD_KEYS.iter().find(|(key, _)| *key == k).ok_or(NotSafe)?;
        if let Some(role) = role {
            push_roles(&mut out, &[*role], v);
        }
        has_of |= k == "of";
    }
    if has_of { Ok(out) } else { Err(NotSafe) }
}

/// 解析不了的调用的动作（方案 3.6）：字面重定向按真实目标与读写角色，再连同全部字面词按
/// [`fallback_candidates`] 抽读 + 写；另加一个承载整条命令文字的执行动作。
pub fn fallback_actions(facts: &PartialFacts, first_word: Option<&str>) -> Vec<PendingAction> {
    let mut out = Vec::new();
    for (kind, target) in &facts.literal_redirects {
        push_redirect(&mut out, *kind, target);
        for cand in fallback_candidates(target) {
            if &cand != target {
                push_argument(&mut out, &cand);
            }
        }
    }
    for w in &facts.literal_words {
        for cand in fallback_candidates(w) {
            push_argument(&mut out, &cand);
        }
    }
    // 词本身是一段命令（eval "…"、bash -c '…'）：再解析一层，里面的字面词与重定向同样抽
    for nested in nested_facts(facts, NESTED_DEPTH) {
        out.extend(
            fallback_actions(&nested, None)
                .into_iter()
                .filter(|a| a.kind != ActionKind::Execute),
        );
    }
    out.push(PendingAction {
        kind: ActionKind::Execute,
        target: PendingTarget::Command {
            program: first_word.unwrap_or("?").to_owned(),
            subcommand: None,
            argv: Vec::new(),
        },
        opaque: false,
        argument: false,
    });
    out
}

/// 嵌套命令文字最多再解析几层。
const NESTED_DEPTH: usize = 2;

/// 含空白或重定向符号的字面词当作命令文字再解析一层，返回其中的事实。
fn nested_facts(facts: &PartialFacts, depth: usize) -> Vec<PartialFacts> {
    if depth == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for w in &facts.literal_words {
        if !w.contains([' ', '\t', '\n', '>', '<', ';', '|', '&']) {
            continue;
        }
        let inner = match iwork_shell::parse(w) {
            Ok(script) => facts_of(&script),
            Err(u) => u.facts,
        };
        out.extend(nested_facts(&inner, depth - 1));
        out.push(inner);
    }
    out
}

/// 已解析成功但归类放弃的脚本：事实取自小 AST 里全部简单命令（扁平，不带参数结构）。
pub fn facts_of(script: &ShellScript) -> PartialFacts {
    let mut facts = PartialFacts::default();
    for c in simple_commands(script) {
        for r in &c.redirects {
            facts
                .literal_redirects
                .push((r.kind, r.target.literal.clone()));
        }
        facts
            .literal_words
            .extend(c.words.iter().map(|w| w.literal.clone()));
    }
    facts
}
