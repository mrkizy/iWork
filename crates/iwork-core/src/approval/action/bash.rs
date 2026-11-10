//! Bash 调用 → 待解析的动作（T07 方案 3.4–3.6）。纯函数，不碰文件系统：路径词原样留着，
//! 由 `PathResolver` 取来事实后在 `normalize` 里换成规范路径。
//!
//! 归类顺序（按顺序查的登记表）：① 整条解析不了的触发 → ② `git` 子命令 → ③ 发布类 → ④ 白名单
//! 参数规则全部满足 → 读 / 写 / 删除 → ⑤ 其余归执行（白名单程序降级时另带保底角色）。②–⑤ 的路径动作
//! 按登记的参数角色精确提取；解析不了的调用另走 [`fallback_actions`]，不认结构、宁可多抽。

use std::collections::BTreeSet;

use iwork_domain::action::ActionKind;
use iwork_shell::{PartialFacts, RedirectKind, ShellScript, SimpleCommand};

use super::commands::{
    self, ArgSpec, DD_KEYS, Floor, GIT_GLOBAL_FLAGS, GIT_RELOCATING, INLINE_CODE_OPTIONS,
    OPAQUE_PROGRAMS, OPAQUE_SUBCOMMANDS, Operands, PUBLISH_COMMANDS, Role, SHELLS,
    UNPARSABLE_PROGRAMS, Value,
};
use super::credential::simple_commands;
use super::find_syntax::{self, FIND};
use super::paths::{
    fallback_candidates, is_path_word, join_basename, split_assignment, value_variants,
};
use super::{PathOrigin, PathQuery, PendingAction, PendingTarget};

/// 这些程序的未登记短选项粘值不让整条解析不了，改为按兜底候选抽读 + 写：`git` 的种类由子命令决定
/// （推送、丢弃改动按种类必问），`git clean -fd`、`git log -n5` 这类组合短选项是日常写法（方案 11.3 验收 4b
/// 要求 `git clean -fd` 是必问而不是解析不了）。候选里任何一个解析到受保护路径仍是底线拒绝。
const GLUED_AS_CANDIDATES: &[&str] = &["git"];

/// 一个子命令「不是安全形态」：归执行，路径按表外规则抽。
struct NotSafe;

/// 整条解析不了的原因。
pub type Why = String;

/// 一条简单命令归类出的动作。
pub struct CommandActions {
    /// 承载这条命令的目标（`Command{program, subcommand, argv}`），沙箱外执行的额外动作用它
    pub command: PendingTarget,
    pub actions: Vec<PendingAction>,
}

/// 已解析的脚本 → 每条简单命令一组动作；任一子命令触发整条解析不了时返回 `Err(原因)`。
pub fn classify_script(script: &ShellScript) -> Result<Vec<CommandActions>, Why> {
    let commands = simple_commands(script);
    if commands.is_empty() {
        return Err("空命令".into());
    }
    let mut out = Vec::new();
    let mut budget = MAX_DIR_CANDIDATES;
    for c in commands {
        let mut actions = Vec::new();
        classify_simple(c, &mut actions, &mut budget)?;
        let words: Vec<&str> = c.words.iter().map(|w| w.literal.as_str()).collect();
        out.push(CommandActions {
            command: command_target(
                &words,
                words.get(1).copied().filter(|a| !a.starts_with('-')),
            ),
            actions,
        });
    }
    Ok(out)
}

/// 沙箱外执行是整条调用的约束（方案 3.8，实施审查 1/F-2）：已有执行动作的命令由归一改种类；
/// 没有执行类动作的命令（白名单读写删、纯重定向、推送、发布、丢弃改动）额外加一个沙箱外执行动作
/// 承载它，原有的路径动作与具体种类动作一个都不动。
pub fn add_outside_sandbox(groups: &mut [CommandActions]) {
    for g in groups {
        if !g.actions.iter().any(|a| a.kind == ActionKind::Execute) {
            g.actions.push(PendingAction {
                kind: ActionKind::OutsideSandbox,
                target: g.command.clone(),
                opaque: false,
                argument: false,
            });
        }
    }
}

fn path(kind: ActionKind, word: &str) -> PendingAction {
    path_with(kind, word, PathOrigin::Real)
}

fn path_with(kind: ActionKind, word: &str, origin: PathOrigin) -> PendingAction {
    PendingAction {
        kind,
        target: PendingTarget::Path(PathQuery {
            literal: word.to_owned(),
            origin,
        }),
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

/// 兜底一类的候选（[`fallback_candidates`] 切出来的、目录目标语义虚构的）：读 + 写，路径走不通时只丢它。
/// `tagged` 决定是否打参数标记（打了的在工作区内不单独再问，方案 3.4 补充）。
fn push_candidate(out: &mut Vec<PendingAction>, word: &str, tagged: bool) {
    if is_path_word(word) {
        for kind in [ActionKind::Read, ActionKind::Write] {
            out.push(PendingAction {
                argument: tagged,
                ..path_with(kind, word, PathOrigin::Hypothetical)
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

fn classify_simple(
    c: &SimpleCommand,
    out: &mut Vec<PendingAction>,
    budget: &mut usize,
) -> Result<(), Why> {
    redirect_actions(c, out);
    let words: Vec<&str> = c.words.iter().map(|w| w.literal.as_str()).collect();
    let Some(&literal_program) = words.first() else {
        return Ok(());
    };
    // 登记表一律按程序标识查：带路径前缀的 git、sudo、npm 与不带前缀的同样归类
    let program = commands::program_id(literal_program);
    let args = &words[1..];

    // ① 整条解析不了
    if UNPARSABLE_PROGRAMS.contains(&program) {
        return Err(format!("{program} 改变执行方式或工作目录"));
    }
    // 程序文本来自参数：shell 与解释器出现在任何位置都算（包装词后面的、`-c` 前后夹着别的选项的），
    // 不看它是第几个词（方案 3.3「精确与保守的分界」）
    for (i, w) in words.iter().enumerate() {
        let name = commands::program_id(w);
        let rest = &words[i + 1..];
        if SHELLS.contains(&name)
            && rest.iter().any(|a| {
                *a == "--command" || (a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
            })
        {
            return Err(format!("{name} -c：程序文本来自参数"));
        }
        if let Some((_, opts)) = INLINE_CODE_OPTIONS.iter().find(|(p, _)| *p == name)
            && rest.iter().any(|a| {
                opts.iter()
                    .any(|o| a == o || (o.len() == 2 && a.starts_with(o)))
            })
        {
            return Err(format!("{name}：程序文本来自参数"));
        }
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
        return extract_external(program, &rest, out);
    }
    // ④ 白名单
    if program == FIND {
        return find_actions(args, out);
    }
    let safe = match program {
        "dd" => dd_form(args),
        _ => match commands::safe_command(program) {
            Some(spec) => safe_form(spec, args),
            None => Err(NotSafe),
        },
    };
    let sub = args.first().copied().filter(|a| !a.starts_with('-'));
    match safe {
        Ok(actions) => {
            out.extend(actions);
            Ok(())
        }
        // ⑤ 执行
        Err(NotSafe) => {
            let execute = command_action(
                ActionKind::Execute,
                &words,
                sub,
                is_opaque(literal_program, sub),
            );
            // 白名单程序降级：只能更严，不丢已知约束（实施审查 1/F-1）——保底角色 + 兜底候选。
            // 保底动作排在执行动作前面：同样是询问时，代表动作取靠前的，必问的原因才显得出来
            let floor_known = match (program, commands::safe_command(program)) {
                ("dd", _) => {
                    dd_floor(args, out);
                    true
                }
                (_, Some(spec)) => {
                    floor_actions(spec.floor, args, out, budget)
                        .map_err(|_| "目录目标候选超过上限".to_string())?;
                    true
                }
                (_, None) => false,
            };
            out.push(execute);
            if floor_known {
                push_all_candidates(args, out);
                Ok(())
            } else {
                extract_external(program, args, out)
            }
        }
    }
}

/// 每个参数的兜底候选（读 + 写，打参数标记）。
fn push_all_candidates(args: &[&str], out: &mut Vec<PendingAction>) {
    for a in args {
        for cand in fallback_candidates(a) {
            push_candidate(out, &cand, true);
        }
    }
}

/// 一次调用里目录目标候选（「词 / 别的词的 basename」）的上限。候选是词数的平方级，每个都要解析一次路径；
/// 超过上限时整条按资源上限拒绝，不截断（截断等于少检查）。
pub const MAX_DIR_CANDIDATES: usize = 4096;

/// 目录目标候选的额度用完了。
#[derive(Debug)]
pub struct TooManyCandidates;

/// 保底角色（方案第 13 节实施审查第 2 轮）：`words` 是程序后面的字面词（选项与选项值都算），每个词按
/// `floor.roles` 产出动作；复制类再对每个词 `d` 与别的词 `s` 产出写入候选 `d / basename(s)`。全是候选：
/// 选项词解析成不存在的名字无害，走不通的只丢它。
fn floor_actions(
    floor: Floor,
    words: &[&str],
    out: &mut Vec<PendingAction>,
    budget: &mut usize,
) -> Result<(), TooManyCandidates> {
    for w in words {
        if is_path_word(w) {
            for r in floor.roles {
                out.push(path_with(role_kind(*r), w, PathOrigin::Hypothetical));
            }
        }
    }
    if !floor.dir_target {
        return Ok(());
    }
    let dirs: BTreeSet<&str> = words.iter().copied().filter(|w| is_path_word(w)).collect();
    for d in &dirs {
        let joined: BTreeSet<String> = dirs
            .iter()
            .filter(|s| *s != d)
            .filter_map(|s| join_basename(d, s))
            .collect();
        *budget = budget.checked_sub(joined.len()).ok_or(TooManyCandidates)?;
        for j in joined {
            out.push(path_with(ActionKind::Write, &j, PathOrigin::Hypothetical));
        }
    }
    Ok(())
}

/// 一串没有命令结构的字面词里，每个白名单程序名（含 `dd`，经包装词的也算）后面的全部字面词按它的保底角色
/// 产出动作（方案第 13 节实施审查第 2 轮）。`find` 另有 [`find_syntax::flat_delete`]。
fn flat_floor<S: AsRef<str>>(
    words: &[S],
    out: &mut Vec<PendingAction>,
    budget: &mut usize,
) -> Result<(), TooManyCandidates> {
    for (i, w) in words.iter().enumerate() {
        let rest: Vec<&str> = words[i + 1..].iter().map(AsRef::as_ref).collect();
        match commands::program_id(w.as_ref()) {
            "dd" => dd_floor(&rest, out),
            name => {
                if let Some(spec) = commands::safe_command(name) {
                    floor_actions(spec.floor, &rest, out, budget)?;
                }
            }
        }
    }
    Ok(())
}

/// `dd` 降级：`if=` 记读、`of=` 记写，不管别的键认不认。
fn dd_floor(args: &[&str], out: &mut Vec<PendingAction>) {
    for a in args {
        if let Some((k, v)) = a.split_once('=')
            && let Some((_, Some(role))) = DD_KEYS.iter().find(|(key, _)| *key == k)
        {
            push_roles(out, &[*role], v);
        }
    }
}

/// 目录目标语义（方案 3.4，实施审查 1/F-7）：目的可能是目录，真正写入的是「目录 / 源文件名」。
/// 目的本身是真实目标；每个源再产出一个假设候选 `d / basename(s)`，`-T` 时不产出。
fn push_dest(
    out: &mut Vec<PendingAction>,
    roles: &[Role],
    dest: &str,
    sources: &[&str],
    maybe_dir: bool,
) {
    push_roles(out, roles, dest);
    if !maybe_dir {
        return;
    }
    for s in sources {
        if let Some(joined) = join_basename(dest, s) {
            for r in roles {
                out.push(path_with(role_kind(*r), &joined, PathOrigin::Hypothetical));
            }
        }
    }
}

/// `find`：按它自己的语法切（parse 成功、全部字面）。只有登记过的词才是安全形态；
/// 带没登记的词整条解析不了（不登记保底角色，收口 9/F-2），删除约束由兜底规则照样产出。
fn find_actions(args: &[&str], out: &mut Vec<PendingAction>) -> Result<(), Why> {
    let call = find_syntax::split(args);
    if call.unregistered {
        return Err("find 带没登记的词".into());
    }
    for r in call.read_targets() {
        push_roles(out, &[Role::Read], r);
    }
    if call.delete {
        for r in call.delete_targets() {
            push_roles(out, &[Role::Delete], r);
        }
    }
    Ok(())
}

/// 一条已解析的简单命令里 `find` 的删除目标（精确规则，用在 parse 成功、归类放弃的调用上）。
pub fn precise_find_deletes(script: &ShellScript) -> Vec<String> {
    let mut out = Vec::new();
    for c in simple_commands(script) {
        let words: Vec<&str> = c.words.iter().map(|w| w.literal.as_str()).collect();
        for (i, w) in words.iter().enumerate() {
            if commands::program_id(w) == FIND {
                let call = find_syntax::split(&words[i + 1..]);
                if call.delete {
                    out.extend(call.delete_targets().into_iter().map(str::to_owned));
                }
            }
        }
    }
    out
}

fn command_target(words: &[&str], subcommand: Option<&str>) -> PendingTarget {
    PendingTarget::Command {
        program: words.first().copied().unwrap_or("?").to_owned(),
        subcommand: subcommand.map(str::to_owned),
        argv: words.iter().skip(1).map(|w| (*w).to_owned()).collect(),
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
        target: command_target(words, subcommand),
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
fn extract_external(program: &str, args: &[&str], out: &mut Vec<PendingAction>) -> Result<(), Why> {
    let registered = commands::path_options(program);
    let mut after_dashdash = false;
    let mut i = 0;
    while i < args.len() {
        let w = args[i];
        if after_dashdash || !w.starts_with('-') || w == "-" {
            push_argument(out, w);
            if !after_dashdash && let Some(v) = split_assignment(w) {
                for v in value_variants(v) {
                    push_argument(out, &v);
                }
            }
        } else if w == "--" {
            after_dashdash = true;
        } else if let Some(long) = w.strip_prefix("--") {
            if let Some((_, v)) = long.split_once('=')
                && !v.is_empty()
            {
                for v in value_variants(v) {
                    push_argument(out, &v);
                }
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
            } else if letters.chars().count() >= 2 {
                if !GLUED_AS_CANDIDATES.contains(&program) {
                    return Err(format!("{program} 的选项 {w} 粘着值（未登记的短选项）"));
                }
                // 组合短选项或粘值分不清：每个切分位置之后的剩余部分都按角色未知的路径抽（与兜底同一规则）
                for cand in fallback_candidates(w) {
                    push_candidate(out, &cand, false);
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
                extract_external("git", &[v], out)?;
            }
            i += 2;
        } else if let Some(v) = w.strip_prefix("-c") {
            extract_external("git", &[v], out)?;
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
    extract_external("git", rest, out)
}

/// `-d`、`-D`、`--delete`，以及含 d / D 的组合短选项（`-Df`）。
fn is_branch_delete(a: &str) -> bool {
    a == "--delete" || (a.starts_with('-') && !a.starts_with("--") && a[1..].contains(['d', 'D']))
}

/// 白名单程序：全部选项登记过、位置参数个数与角色对得上，才是安全形态。
fn safe_form(spec: &ArgSpec, args: &[&str]) -> Result<Vec<PendingAction>, NotSafe> {
    let mut out = Vec::new();
    let mut operands: Vec<Operand> = Vec::new();
    // `-t DIR` 给出的目的目录：位置参数解析完后才知道源，再按目录目标语义展开
    let mut target_dirs: Vec<String> = Vec::new();
    let mut pattern_given = false;
    let mut all_write = false;
    let mut no_target_dir = false;
    let mut after_dashdash = false;
    let mut apply = |value: Value, v: &str, out: &mut Vec<PendingAction>| match value {
        Value::Word => {}
        Value::Path(roles) => push_roles(out, roles, v),
        Value::TargetDir => target_dirs.push(v.to_owned()),
        Value::PatternWord => pattern_given = true,
        Value::PatternFile => {
            pattern_given = true;
            push_roles(out, &[Role::Read], v);
        }
        Value::AllWriteFlag | Value::NoTargetDirFlag => unreachable!("不取值的选项不走这里"),
    };
    let mut i = 0;
    while i < args.len() {
        let w = args[i];
        if after_dashdash || !w.starts_with('-') || w == "-" {
            // `--` 之前的 `-` 可能是标准流：只在只读角色的位置当标准输入（见 push_operand）
            operands.push(Operand {
                word: w,
                maybe_stdin: w == "-" && !after_dashdash,
            });
        } else if w == "--" {
            after_dashdash = true;
        } else if let Some(long) = w.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            if inline.is_none() && spec.long_flags.contains(&name) {
                no_target_dir |= name == "no-target-directory";
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
                if matches!(value, Value::AllWriteFlag | Value::NoTargetDirFlag) {
                    all_write |= *value == Value::AllWriteFlag;
                    no_target_dir |= *value == Value::NoTargetDirFlag;
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
            push_operand(&mut out, &[Role::Write], o);
        }
        Role::Write
    } else {
        match spec.operands {
            Operands::All(roles) => {
                for o in &operands {
                    push_operand(&mut out, roles, o);
                }
                roles.first().copied().unwrap_or(Role::Read)
            }
            Operands::After { skip, roles } => {
                let skip = if pattern_given { 0 } else { skip };
                if operands.len() < skip {
                    return Err(NotSafe);
                }
                for o in &operands[skip..] {
                    push_operand(&mut out, roles, o);
                }
                roles[0]
            }
            Operands::SourcesThenDest { source, dest } => {
                let (sources, dests) = if target_dirs.is_empty() {
                    if operands.len() < 2 {
                        return Err(NotSafe);
                    }
                    operands.split_at(operands.len() - 1)
                } else {
                    (&operands[..], &[][..])
                };
                if sources.is_empty() {
                    return Err(NotSafe);
                }
                for o in sources {
                    push_operand(&mut out, source, o);
                }
                let source_words: Vec<&str> = sources.iter().map(|o| o.word).collect();
                for d in dests
                    .iter()
                    .map(|o| o.word)
                    .chain(target_dirs.iter().map(String::as_str))
                {
                    push_dest(&mut out, dest, d, &source_words, !no_target_dir);
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

/// 白名单程序的一个位置参数。
struct Operand<'a> {
    word: &'a str,
    /// 是 `--` 之前的 `-`：在只读位置是标准输入
    maybe_stdin: bool,
}

/// 只读位置的 `-` 是标准输入，不是路径；写或删除位置的 `-` 是标准输出还是名为 `-` 的文件拿不准，
/// 按文件保守处理（审查 1/F-4，`tee -a -`）。
fn push_operand(out: &mut Vec<PendingAction>, roles: &[Role], o: &Operand) {
    if o.maybe_stdin && roles.iter().all(|r| *r == Role::Read) {
        return;
    }
    push_roles(out, roles, o.word);
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

/// 解析不了的调用的动作（方案 3.6）。路径来源只有第一遍的扁平事实：字面重定向按真实目标与读写角色，
/// 再连同全部字面词（原文与队列里的）按 [`fallback_candidates`] 抽读 + 写；`find … -delete` 的删除约束
/// 不随解析不了丢掉：parse 成功时用精确切法的结果（`precise_find`），否则对原文的字面词用保守规则；
/// 队列里的词恒用保守规则。另有一个承载整条命令文字的执行动作。路径全是候选：走不通的只丢它自己。
///
/// 第二个返回值为真表示目录目标候选超过上限（[`MAX_DIR_CANDIDATES`]），调用方按资源上限拒绝。
pub fn fallback_actions(
    facts: &PartialFacts,
    precise_find: Option<&[String]>,
) -> (Vec<PendingAction>, bool) {
    // 承载命令文字的执行动作排第一：同样是询问时代表动作取靠前的，原因显示为「解析不了」
    let mut out = vec![PendingAction {
        kind: ActionKind::Execute,
        target: PendingTarget::Command {
            // 程序名取第一个不是 name=value 的词（前置赋值在命令名前面）
            program: facts
                .literal_words
                .iter()
                .find(|w| split_assignment(w).is_none())
                .or(facts.literal_words.first())
                .map_or("?", String::as_str)
                .to_owned(),
            subcommand: None,
            argv: Vec::new(),
        },
        opaque: false,
        argument: false,
    }];
    for (kind, target) in &facts.literal_redirects {
        if kind.reads() {
            out.push(path_with(
                ActionKind::Read,
                target,
                PathOrigin::Hypothetical,
            ));
        }
        if kind.writes() {
            out.push(path_with(
                ActionKind::Write,
                target,
                PathOrigin::Hypothetical,
            ));
        }
        for cand in fallback_candidates(target) {
            if &cand != target {
                push_candidate(&mut out, &cand, false);
            }
        }
    }
    for w in facts.literal_words.iter().chain(&facts.queued_words) {
        for cand in fallback_candidates(w) {
            push_candidate(&mut out, &cand, false);
        }
    }
    let original_deletes = match precise_find {
        Some(precise) => precise.to_vec(),
        None => find_syntax::flat_delete(&facts.literal_words),
    };
    for d in original_deletes
        .iter()
        .chain(&find_syntax::flat_delete(&facts.queued_words))
    {
        out.push(path_with(ActionKind::Delete, d, PathOrigin::Hypothetical));
    }
    // 白名单程序名后面的词按保底角色（原文与队列里的词各自扁平处理，没有命令边界）
    let mut budget = MAX_DIR_CANDIDATES;
    let too_many = flat_floor(&facts.literal_words, &mut out, &mut budget).is_err()
        || flat_floor(&facts.queued_words, &mut out, &mut budget).is_err();
    (out, too_many)
}
