//! Bash 调用 → 待解析的动作（T07 方案 3.4–3.6）。纯函数，不碰文件系统：路径词原样留着，
//! 由 `PathResolver` 取来事实后在 `normalize` 里换成规范路径。
//!
//! 归类顺序（按顺序查的登记表）：① 整条解析不了的触发 → ② `git` 子命令 → ③ 发布类 → ④ 白名单
//! 参数规则全部满足 → 读 / 写 / 删除 → ⑤ 其余归执行（白名单程序降级时另带保底角色）。②–⑤ 的路径动作
//! 按登记的参数角色精确提取；解析不了的调用另走 [`fallback_actions`]，不认结构、宁可多抽。
//!
//! 这里**不直接构造路径动作**：全部动作经 [`Expander`] 产出，路径去重、整条调用一份额度、生成之前先扣
//! （5/F-1）。新加候选来源也只能调它的方法，否则额度就有了绕开的入口。

use std::collections::BTreeSet;

use iwork_domain::action::ActionKind;
use iwork_shell::{PartialFacts, RedirectKind, ShellScript, SimpleCommand};

use super::commands::{
    self, ArgSpec, DD_KEYS, Floor, GIT_GLOBAL_FLAGS, GIT_RELOCATING, INLINE_CODE_OPTIONS,
    OPAQUE_PROGRAMS, OPAQUE_SUBCOMMANDS, Operands, PUBLISH_COMMANDS, Role, SHELLS,
    UNPARSABLE_PROGRAMS, Value,
};
use super::credential::simple_commands;
use super::expansion::{Budget, Exhausted, Expander};
use super::find_syntax::{self, FIND};
use super::paths::{split_assignment, value_variants};
use super::{PathOrigin, PendingAction, PendingTarget};

/// 这些程序的未登记短选项粘值不让整条解析不了，改为按兜底候选抽读 + 写：`git` 的种类由子命令决定
/// （推送、丢弃改动按种类必问），`git clean -fd`、`git log -n5` 这类组合短选项是日常写法（方案 11.3 验收 4b
/// 要求 `git clean -fd` 是必问而不是解析不了）。候选里任何一个解析到受保护路径仍是底线拒绝。
const GLUED_AS_CANDIDATES: &[&str] = &["git"];

/// 整条解析不了的原因。
pub type Why = String;

/// 一条简单命令归类出的动作。
pub struct CommandActions {
    /// 承载这条命令的目标（`Command{program, subcommand, argv}`），沙箱外执行的额外动作用它
    pub command: PendingTarget,
    pub actions: Vec<PendingAction>,
}

/// 已解析的脚本 → 每条简单命令一组动作；任一子命令触发整条解析不了时返回 `Err(原因)`。
/// 整条调用一份额度（[`Budget`]），每条命令的动作都经 [`Expander`] 产出；额度用完整条解析不了，
/// 兜底再展开一次仍超额就按资源上限拒。
pub fn classify_script(script: &ShellScript) -> Result<Vec<CommandActions>, Why> {
    let commands = simple_commands(script);
    if commands.is_empty() {
        return Err("空命令".into());
    }
    let mut out = Vec::new();
    let mut budget = Budget::default();
    for c in commands {
        let mut e = Expander::new(&mut budget);
        classify_simple(c, &mut e)?;
        let words: Vec<&str> = c.words.iter().map(|w| w.literal.as_str()).collect();
        out.push(CommandActions {
            command: command_target(
                &words,
                words.get(1).copied().filter(|a| !a.starts_with('-')),
            ),
            actions: e.out,
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

fn role_kind(role: Role) -> ActionKind {
    match role {
        Role::Read => ActionKind::Read,
        Role::Write => ActionKind::Write,
        Role::Delete => ActionKind::Delete,
    }
}

fn role_kinds(roles: &[Role]) -> Vec<ActionKind> {
    roles.iter().map(|r| role_kind(*r)).collect()
}

/// 额度用完：整条调用解析不了（兜底再展开一次，仍超额就按资源上限拒）。
fn exhausted(_: Exhausted) -> Why {
    "路径候选超过上限".into()
}

/// 精确角色的真实目标。
fn push_roles(e: &mut Expander, roles: &[Role], word: &str) -> Result<(), Exhausted> {
    for r in roles {
        e.push(role_kind(*r), word, PathOrigin::Real, false)?;
    }
    Ok(())
}

/// 角色未知的参数路径：既记读又记写（不知道它会读还是写，两种都要过底线与越界）。
fn push_argument(e: &mut Expander, word: &str) -> Result<(), Exhausted> {
    for kind in [ActionKind::Read, ActionKind::Write] {
        e.push(kind, word, PathOrigin::Real, true)?;
    }
    Ok(())
}

/// 一个词的全部兜底候选（读 + 写，走不通时只丢它）；`tagged` 决定是否打参数标记。
fn push_candidates(e: &mut Expander, word: &str, tagged: bool) -> Result<(), Exhausted> {
    for cand in e.candidates(word)? {
        for kind in [ActionKind::Read, ActionKind::Write] {
            e.push(kind, &cand, PathOrigin::Hypothetical, tagged)?;
        }
    }
    Ok(())
}

fn push_redirect(e: &mut Expander, kind: RedirectKind, target: &str) -> Result<(), Exhausted> {
    if kind.reads() {
        push_roles(e, &[Role::Read], target)?;
    }
    if kind.writes() {
        push_roles(e, &[Role::Write], target)?;
    }
    Ok(())
}

/// shell 与解释器出现在任何位置、后面任何位置有程序文本选项（方案 3.3「精确与保守的分界」）：
/// 从后往前一遍，记下后面出现过的词，不为每个位置重扫后缀（5/F-1 同类）。
fn inline_program_text(words: &[&str]) -> Option<String> {
    let mut shell_c_after = false;
    let mut after: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut after_prefix2: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for w in words.iter().rev() {
        let name = commands::program_id(w);
        if SHELLS.contains(&name) && shell_c_after {
            return Some(format!("{name} -c：程序文本来自参数"));
        }
        if let Some((_, opts)) = INLINE_CODE_OPTIONS.iter().find(|(p, _)| *p == name)
            && opts
                .iter()
                .any(|o| after.contains(o) || (o.len() == 2 && after_prefix2.contains(o)))
        {
            return Some(format!("{name}：程序文本来自参数"));
        }
        shell_c_after |=
            *w == "--command" || (w.starts_with('-') && !w.starts_with("--") && w.contains('c'));
        after.insert(w);
        if let Some(p) = w.get(..2) {
            after_prefix2.insert(p);
        }
    }
    None
}

fn classify_simple(c: &SimpleCommand, e: &mut Expander) -> Result<(), Why> {
    for r in &c.redirects {
        push_redirect(e, r.kind, &r.target.literal).map_err(exhausted)?;
    }
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
    if let Some(why) = inline_program_text(&words) {
        return Err(why);
    }

    // ② git
    if program == "git" {
        return classify_git(&words, e);
    }
    // ③ 发布类
    if let Some((start, seq)) = publish_match(program, args) {
        let consumed = start..start + seq.len();
        e.push_action(command_action(
            ActionKind::Publish,
            &words,
            Some(seq[0]),
            false,
        ))
        .map_err(exhausted)?;
        let rest: Vec<&str> = args
            .iter()
            .enumerate()
            .filter(|(i, _)| !consumed.contains(i))
            .map(|(_, a)| *a)
            .collect();
        return extract_external(program, &rest, e);
    }
    // ④ 白名单
    if program == FIND {
        return find_actions(args, e);
    }
    let safe = {
        let mut trial = e.child();
        let result = match program {
            "dd" => dd_form(args, &mut trial),
            _ => match commands::safe_command(program) {
                Some(spec) => safe_form(spec, args, &mut trial),
                None => Err(Stop::NotSafe),
            },
        };
        result.map(|()| trial.out)
    };
    let sub = args.first().copied().filter(|a| !a.starts_with('-'));
    match safe {
        Ok(actions) => {
            e.adopt(actions);
            Ok(())
        }
        Err(Stop::Exhausted) => Err(exhausted(Exhausted)),
        // ⑤ 执行
        Err(Stop::NotSafe) => {
            let execute = command_action(
                ActionKind::Execute,
                &words,
                sub,
                is_opaque(literal_program, sub),
            );
            // 白名单程序降级：只能更严，不丢已知约束（实施审查 1/F-1）——保底角色 + 兜底候选。
            // 保底动作排在执行动作前面：同样是询问时，代表动作取靠前的，必问的原因才显得出来
            let floor = match (program, commands::safe_command(program)) {
                ("dd", _) => Some(FloorStart::Dd),
                (_, Some(spec)) => Some(FloorStart::Program(spec.floor)),
                (_, None) => None,
            };
            let Some(floor) = floor else {
                e.push_action(execute).map_err(exhausted)?;
                return extract_external(program, args, e);
            };
            floor_scan(e, args, Some(floor), false).map_err(exhausted)?;
            e.push_action(execute).map_err(exhausted)?;
            for a in args {
                push_candidates(e, a, true).map_err(exhausted)?;
            }
            Ok(())
        }
    }
}

/// 白名单归类停在哪：不是安全形态（降级），或额度用完。
enum Stop {
    NotSafe,
    Exhausted,
}

impl From<Exhausted> for Stop {
    fn from(_: Exhausted) -> Self {
        Self::Exhausted
    }
}

/// 保底扫描从哪里开始生效。
#[derive(Debug, Clone, Copy)]
enum FloorStart {
    Program(Floor),
    Dd,
}

/// 保底角色（方案第 13 节实施审查第 2 轮、3/F-1、3/F-2）：一遍扫过 `words`，每个词的**完整候选集合**
/// （整词 + [`super::paths::fallback_candidates`] 拆出的值：`--target-directory=.` 的 `.`、`-t.` 的 `.`）
/// 按当前生效的角色产出动作；复制类再在全部候选之间配出「候选 / 别的候选的 basename」写入候选。
/// `start` 是降级程序自己的保底；`scan_programs` 时（解析不了的扁平词）词里每出现一个白名单程序名
/// （含 `dd`、经包装词也算），它的保底就并进后面词的角色——角色只增不减，一遍扫完。
/// 全是候选：选项词解析成不存在的名字无害，走不通的只丢它。去重与额度由 [`Expander`] 管。
fn floor_scan<S: AsRef<str>>(
    e: &mut Expander,
    words: &[S],
    start: Option<FloorStart>,
    scan_programs: bool,
) -> Result<(), Exhausted> {
    let mut roles: BTreeSet<Role> = BTreeSet::new();
    let mut dir_target = false;
    let mut dd = false;
    let absorb = |start: FloorStart, roles: &mut BTreeSet<Role>, dir: &mut bool, dd: &mut bool| {
        match start {
            FloorStart::Program(f) => {
                roles.extend(f.roles.iter().copied());
                *dir |= f.dir_target;
            }
            FloorStart::Dd => *dd = true,
        }
    };
    if let Some(start) = start {
        absorb(start, &mut roles, &mut dir_target, &mut dd);
    }
    let mut dir_candidates: BTreeSet<String> = BTreeSet::new();
    for w in words {
        let w = w.as_ref();
        if !roles.is_empty() {
            let cands = e.candidates(w)?;
            for c in &cands {
                for r in &roles {
                    e.push(role_kind(*r), c, PathOrigin::Hypothetical, false)?;
                }
            }
            if dir_target {
                dir_candidates.extend(cands);
            }
        }
        if dd
            && let Some((k, v)) = w.split_once('=')
            && let Some((_, Some(role))) = DD_KEYS.iter().find(|(key, _)| *key == k)
        {
            e.push(role_kind(*role), v, PathOrigin::Hypothetical, false)?;
        }
        if scan_programs {
            match commands::program_id(w) {
                "dd" => absorb(FloorStart::Dd, &mut roles, &mut dir_target, &mut dd),
                name => {
                    if let Some(spec) = commands::safe_command(name) {
                        absorb(
                            FloorStart::Program(spec.floor),
                            &mut roles,
                            &mut dir_target,
                            &mut dd,
                        );
                    }
                }
            }
        }
    }
    let dirs: Vec<&str> = dir_candidates.iter().map(String::as_str).collect();
    e.dir_pairs(&[ActionKind::Write], &dirs, &dirs, true)
}

/// `find`：按它自己的语法切（parse 成功、全部字面）。只有登记过的词才是安全形态；
/// 带没登记的词整条解析不了（不登记保底角色，收口 9/F-2），删除约束由兜底规则照样产出。
fn find_actions(args: &[&str], e: &mut Expander) -> Result<(), Why> {
    let call = find_syntax::split(args);
    if call.unregistered {
        return Err("find 带没登记的词".into());
    }
    for r in call.read_targets() {
        push_roles(e, &[Role::Read], r).map_err(exhausted)?;
    }
    if call.delete {
        for r in call.delete_targets() {
            push_roles(e, &[Role::Delete], r).map_err(exhausted)?;
        }
    }
    Ok(())
}

/// `find` 的删除目标经 `push_delete` 交出（精确规则，parse 成功、归类放弃的调用）。每条简单命令里：只有一个
/// `find` 时按它的语法切；有几个 `find`（后面的 `find` 是前面某个的参数，结构已经不可靠）时改用保守规则，
/// 不为每个 `find` 重扫后缀（5/F-1）——保守规则的结果是各个精确切法结果的并集的超集。
fn precise_find_deletes(
    script: &ShellScript,
    push_delete: &mut impl FnMut(&str) -> Result<(), Exhausted>,
) -> Result<(), Exhausted> {
    for c in simple_commands(script) {
        let words: Vec<&str> = c.words.iter().map(|w| w.literal.as_str()).collect();
        let mut finds = words
            .iter()
            .enumerate()
            .filter(|(_, w)| commands::program_id(w) == FIND)
            .map(|(i, _)| i);
        match (finds.next(), finds.next()) {
            (Some(i), None) => {
                let call = find_syntax::split(&words[i + 1..]);
                if call.delete {
                    for t in call.delete_targets() {
                        push_delete(t)?;
                    }
                }
            }
            (Some(_), Some(_)) => {
                if let Some(start) = find_syntax::flat_delete_start(&words) {
                    for t in &words[start..] {
                        push_delete(t)?;
                    }
                    push_delete(".")?;
                }
            }
            _ => {}
        }
    }
    Ok(())
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
fn extract_external(program: &str, args: &[&str], e: &mut Expander) -> Result<(), Why> {
    let registered = commands::path_options(program);
    let mut after_dashdash = false;
    let mut i = 0;
    while i < args.len() {
        let w = args[i];
        if after_dashdash || !w.starts_with('-') || w == "-" {
            push_argument(e, w).map_err(exhausted)?;
            if !after_dashdash && let Some(v) = split_assignment(w) {
                for v in value_variants(v) {
                    push_argument(e, &v).map_err(exhausted)?;
                }
            }
        } else if w == "--" {
            after_dashdash = true;
        } else if let Some(long) = w.strip_prefix("--") {
            if let Some((_, v)) = long.split_once('=')
                && !v.is_empty()
            {
                for v in value_variants(v) {
                    push_argument(e, &v).map_err(exhausted)?;
                }
            }
        } else {
            let letters = &w[1..];
            let first = letters.chars().next().expect("w 以 - 开头且不是单独的 -");
            if let Some((_, roles)) = registered.iter().find(|(c, _)| *c == first) {
                let glued = &letters[first.len_utf8()..];
                if !glued.is_empty() {
                    push_roles(e, roles, glued).map_err(exhausted)?;
                } else if let Some(v) = args.get(i + 1) {
                    push_roles(e, roles, v).map_err(exhausted)?;
                    i += 1;
                }
            } else if letters.chars().count() >= 2 {
                if !GLUED_AS_CANDIDATES.contains(&program) {
                    return Err(format!("{program} 的选项 {w} 粘着值（未登记的短选项）"));
                }
                // 组合短选项或粘值分不清：每个切分位置之后的剩余部分都按角色未知的路径抽（与兜底同一规则，
                // 经额度入口，5/F-1）
                push_candidates(e, w, false).map_err(exhausted)?;
            }
        }
        i += 1;
    }
    Ok(())
}

fn classify_git(words: &[&str], e: &mut Expander) -> Result<(), Why> {
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
                extract_external("git", &[v], e)?;
            }
            i += 2;
        } else if let Some(v) = w.strip_prefix("-c") {
            extract_external("git", &[v], e)?;
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
    let action = if kind == ActionKind::Push {
        let remote = rest
            .iter()
            .find(|a| !a.starts_with('-'))
            .copied()
            .unwrap_or("origin");
        PendingAction {
            kind,
            target: PendingTarget::Remote(remote.to_owned()),
            opaque: false,
            argument: false,
        }
    } else {
        // 丢弃改动的 git 命令目标是命令，不是路径：必问层按种类命中，不会撞上「删除工作区」底线（F-13）
        command_action(kind, words, sub, false)
    };
    e.push_action(action).map_err(exhausted)?;
    extract_external("git", rest, e)
}

/// `-d`、`-D`、`--delete`，以及含 d / D 的组合短选项（`-Df`）。
fn is_branch_delete(a: &str) -> bool {
    a == "--delete" || (a.starts_with('-') && !a.starts_with("--") && a[1..].contains(['d', 'D']))
}

/// 白名单程序：全部选项登记过、位置参数个数与角色对得上，才是安全形态。动作写进试探用的子收集器，
/// 不是安全形态时整组丢掉（已扣的额度不退，只会更早失败关闭）。
fn safe_form(spec: &ArgSpec, args: &[&str], e: &mut Expander) -> Result<(), Stop> {
    let mut operands: Vec<Operand> = Vec::new();
    // `-t DIR` 给出的目的目录：位置参数解析完后才知道源，再按目录目标语义展开
    let mut target_dirs: Vec<&str> = Vec::new();
    let mut glued_values: Vec<String> = Vec::new();
    let mut pattern_given = false;
    let mut all_write = false;
    let mut no_target_dir = false;
    let mut after_dashdash = false;
    let mut i = 0;
    // 选项值：先记下（粘值要拥有），解析完再按角色产出
    let mut values: Vec<(Value, usize, bool)> = Vec::new();
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
                match inline {
                    Some(v) => {
                        glued_values.push(v.to_owned());
                        values.push((*value, glued_values.len() - 1, true));
                    }
                    None => {
                        i += 1;
                        args.get(i).ok_or(Stop::NotSafe)?;
                        values.push((*value, i, false));
                    }
                }
            } else {
                return Err(Stop::NotSafe);
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
                    return Err(Stop::NotSafe);
                };
                if matches!(value, Value::AllWriteFlag | Value::NoTargetDirFlag) {
                    all_write |= *value == Value::AllWriteFlag;
                    no_target_dir |= *value == Value::NoTargetDirFlag;
                    j += 1;
                    continue;
                }
                let glued: String = letters[j + 1..].iter().collect();
                if glued.is_empty() {
                    i += 1;
                    args.get(i).ok_or(Stop::NotSafe)?;
                    values.push((*value, i, false));
                } else {
                    glued_values.push(glued);
                    values.push((*value, glued_values.len() - 1, true));
                }
                break;
            }
        }
        i += 1;
    }
    if spec.max_operands.is_some_and(|m| operands.len() > m) {
        return Err(Stop::NotSafe);
    }
    for (value, at, glued) in &values {
        let v: &str = if *glued {
            &glued_values[*at]
        } else {
            args[*at]
        };
        match value {
            Value::Word => {}
            Value::Path(roles) => push_roles(e, roles, v)?,
            Value::TargetDir => target_dirs.push(v),
            Value::PatternWord => pattern_given = true,
            Value::PatternFile => {
                pattern_given = true;
                push_roles(e, &[Role::Read], v)?;
            }
            Value::AllWriteFlag | Value::NoTargetDirFlag => unreachable!("不取值的选项不记下"),
        }
    }
    let primary = if all_write {
        for o in &operands {
            push_operand(e, &[Role::Write], o)?;
        }
        Role::Write
    } else {
        match spec.operands {
            Operands::All(roles) => {
                for o in &operands {
                    push_operand(e, roles, o)?;
                }
                roles.first().copied().unwrap_or(Role::Read)
            }
            Operands::After { skip, roles } => {
                let skip = if pattern_given { 0 } else { skip };
                if operands.len() < skip {
                    return Err(Stop::NotSafe);
                }
                for o in &operands[skip..] {
                    push_operand(e, roles, o)?;
                }
                roles[0]
            }
            Operands::SourcesThenDest { source, dest } => {
                let (sources, dests) = if target_dirs.is_empty() {
                    if operands.len() < 2 {
                        return Err(Stop::NotSafe);
                    }
                    operands.split_at(operands.len() - 1)
                } else {
                    (&operands[..], &[][..])
                };
                if sources.is_empty() {
                    return Err(Stop::NotSafe);
                }
                for o in sources {
                    push_operand(e, source, o)?;
                }
                // 目录目标语义（方案 3.4，实施审查 1/F-7）：目的本身是真实目标；目的可能是目录时再按
                // 唯一的（目的, 源文件名）组合产出假设候选，`-T` 时不产出（5/F-1：不按 目的 × 源 逐个拼）
                let dests: Vec<&str> = dests.iter().map(|o| o.word).chain(target_dirs).collect();
                for d in &dests {
                    push_roles(e, dest, d)?;
                }
                if !no_target_dir {
                    let source_words: Vec<&str> = sources.iter().map(|o| o.word).collect();
                    e.dir_pairs(&role_kinds(dest), &dests, &source_words, false)?;
                }
                dest[0]
            }
        }
    };
    if e.out.is_empty() {
        // 读标准输入等：没有路径目标，仍记一个动作，审计看得到
        e.push_action(PendingAction {
            kind: role_kind(primary),
            target: PendingTarget::None,
            opaque: false,
            argument: false,
        })?;
    }
    Ok(())
}

/// 白名单程序的一个位置参数。
struct Operand<'a> {
    word: &'a str,
    /// 是 `--` 之前的 `-`：在只读位置是标准输入
    maybe_stdin: bool,
}

/// 只读位置的 `-` 是标准输入，不是路径；写或删除位置的 `-` 是标准输出还是名为 `-` 的文件拿不准，
/// 按文件保守处理（审查 1/F-4，`tee -a -`）。
fn push_operand(e: &mut Expander, roles: &[Role], o: &Operand) -> Result<(), Exhausted> {
    if o.maybe_stdin && roles.iter().all(|r| *r == Role::Read) {
        return Ok(());
    }
    push_roles(e, roles, o.word)
}

/// `dd`：只认登记的 `key=value`，且要有 `of=`。
fn dd_form(args: &[&str], e: &mut Expander) -> Result<(), Stop> {
    let mut has_of = false;
    for a in args {
        let (k, _) = a.split_once('=').ok_or(Stop::NotSafe)?;
        DD_KEYS
            .iter()
            .find(|(key, _)| *key == k)
            .ok_or(Stop::NotSafe)?;
        has_of |= k == "of";
    }
    if !has_of {
        return Err(Stop::NotSafe);
    }
    for a in args {
        if let Some((k, v)) = a.split_once('=')
            && let Some((_, Some(role))) = DD_KEYS.iter().find(|(key, _)| *key == k)
        {
            push_roles(e, &[*role], v)?;
        }
    }
    Ok(())
}

/// 解析不了的调用的动作（方案 3.6）。路径来源只有第一遍的扁平事实：字面重定向按真实目标与读写角色，
/// 再连同全部字面词（原文与队列里的）按 [`fallback_candidates`] 抽读 + 写；`find … -delete` 的删除约束
/// 不随解析不了丢掉：parse 成功时用精确切法的结果（`precise_find`），否则对原文的字面词用保守规则；
/// 队列里的词恒用保守规则。另有一个承载整条命令文字的执行动作。路径全是候选：走不通的只丢它自己。
///
/// 第二个返回值为真表示路径候选超过额度（[`super::expansion`]），调用方按资源上限拒绝。
pub fn fallback_actions(
    facts: &PartialFacts,
    precise_find: Option<&ShellScript>,
) -> (Vec<PendingAction>, bool) {
    let mut budget = Budget::default();
    let mut e = Expander::new(&mut budget);
    let expanded = fallback_paths(&mut e, facts, precise_find);
    (e.out, expanded.is_err())
}

fn fallback_paths(
    e: &mut Expander,
    facts: &PartialFacts,
    precise_find: Option<&ShellScript>,
) -> Result<(), Exhausted> {
    // 承载命令文字的执行动作排第一：同样是询问时代表动作取靠前的，原因显示为「解析不了」
    e.push_action(PendingAction {
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
    })?;
    let hypo = PathOrigin::Hypothetical;
    for (kind, target) in &facts.literal_redirects {
        if kind.reads() {
            e.push(ActionKind::Read, target, hypo, false)?;
        }
        if kind.writes() {
            e.push(ActionKind::Write, target, hypo, false)?;
        }
        push_candidates(e, target, false)?;
    }
    for w in facts.literal_words.iter().chain(&facts.queued_words) {
        push_candidates(e, w, false)?;
    }
    let mut delete = |t: &str| e.push(ActionKind::Delete, t, hypo, false);
    match precise_find {
        Some(script) => precise_find_deletes(script, &mut delete)?,
        None => flat_deletes(&facts.literal_words, &mut delete)?,
    }
    flat_deletes(&facts.queued_words, &mut delete)?;
    // 白名单程序名后面的词按保底角色（原文与队列里的词各自扁平处理，没有命令边界）
    floor_scan(e, &facts.literal_words, None, true)?;
    floor_scan(e, &facts.queued_words, None, true)
}

/// 保守规则（[`find_syntax::flat_delete_start`]）：`find` 之后的每个字面词与 cwd 都是删除候选。
fn flat_deletes(
    words: &[String],
    push_delete: &mut impl FnMut(&str) -> Result<(), Exhausted>,
) -> Result<(), Exhausted> {
    if let Some(start) = find_syntax::flat_delete_start(words) {
        for w in &words[start..] {
            push_delete(w)?;
        }
        push_delete(".")?;
    }
    Ok(())
}
