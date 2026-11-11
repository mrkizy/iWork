//! 第二遍：`brush-parser` 的 AST → 小 AST。表达不了的形态返回 `Err(原因)`，不丢字段。

use brush_parser::ast;

use crate::{
    CommandNode, Connector, ListItem, Redirect, RedirectKind, ShellScript, SimpleCommand, Word,
    literal_value,
};

type R<T> = Result<T, String>;

pub(crate) fn program(p: &ast::Program) -> R<ShellScript> {
    let mut items = Vec::new();
    for list in &p.complete_commands {
        items.extend(compound_list(list)?.items);
    }
    finish(items)
}

/// 最后一项的 `;` 不连接任何东西，记为 `None`。
fn finish(mut items: Vec<ListItem>) -> R<ShellScript> {
    if let Some(last) = items.last_mut()
        && last.connector == Some(Connector::Seq)
    {
        last.connector = None;
    }
    Ok(ShellScript { items })
}

fn compound_list(list: &ast::CompoundList) -> R<ShellScript> {
    let mut items = Vec::new();
    for ast::CompoundListItem(and_or, sep) in &list.0 {
        let mut pipelines = vec![&and_or.first];
        let mut connectors = Vec::new();
        for next in &and_or.additional {
            match next {
                ast::AndOr::And(p) => {
                    connectors.push(Connector::And);
                    pipelines.push(p);
                }
                ast::AndOr::Or(p) => {
                    connectors.push(Connector::Or);
                    pipelines.push(p);
                }
            }
        }
        connectors.push(match sep {
            ast::SeparatorOperator::Async => Connector::Async,
            ast::SeparatorOperator::Sequence => Connector::Seq,
        });
        for (p, connector) in pipelines.into_iter().zip(connectors) {
            items.push(ListItem {
                pipeline: pipeline(p)?,
                negated: p.bang,
                connector: Some(connector),
            });
        }
    }
    finish(items)
}

fn pipeline(p: &ast::Pipeline) -> R<Vec<CommandNode>> {
    if p.timed.is_some() {
        return Err("time 计时".into());
    }
    p.seq.iter().map(command).collect()
}

fn command(c: &ast::Command) -> R<CommandNode> {
    match c {
        ast::Command::Simple(s) => simple(s).map(CommandNode::Simple),
        ast::Command::Compound(compound, redirects) => {
            if redirects.as_ref().is_some_and(|r| !r.0.is_empty()) {
                return Err("复合命令上的重定向".into());
            }
            compound_command(compound)
        }
        ast::Command::Function(_) => Err("函数定义".into()),
        ast::Command::ExtendedTest(..) => Err("[[ ]] 测试命令".into()),
    }
}

fn compound_command(c: &ast::CompoundCommand) -> R<CommandNode> {
    use ast::CompoundCommand as C;
    match c {
        C::BraceGroup(g) => compound_list(&g.list).map(CommandNode::BraceGroup),
        C::Subshell(s) => compound_list(&s.list).map(CommandNode::Subshell),
        C::IfClause(i) => {
            let mut branches = vec![compound_list(&i.condition)?, compound_list(&i.then)?];
            for e in i.elses.iter().flatten() {
                if let Some(cond) = &e.condition {
                    branches.push(compound_list(cond)?);
                }
                branches.push(compound_list(&e.body)?);
            }
            Ok(CommandNode::If { branches })
        }
        C::WhileClause(w) | C::UntilClause(w) => Ok(CommandNode::Loop {
            condition: compound_list(&w.0)?,
            body: compound_list(&w.1.list)?,
        }),
        C::ForClause(_) => Err("for 循环（值列表要展开）".into()),
        C::ArithmeticForClause(_) => Err("算术 for 循环".into()),
        C::CaseClause(_) => Err("case（选择词要展开）".into()),
        C::Arithmetic(_) => Err("算术命令".into()),
        C::Coprocess(_) => Err("协进程".into()),
    }
}

fn simple(s: &ast::SimpleCommand) -> R<SimpleCommand> {
    let mut words = Vec::new();
    let mut redirects = Vec::new();
    for item in s.prefix.iter().flat_map(|p| &p.0) {
        match item {
            ast::CommandPrefixOrSuffixItem::IoRedirect(r) => redirects.extend(redirect(r)?),
            ast::CommandPrefixOrSuffixItem::AssignmentWord(..) => {
                return Err("前置环境变量赋值".into());
            }
            ast::CommandPrefixOrSuffixItem::ProcessSubstitution(..) => {
                return Err("进程替换".into());
            }
            ast::CommandPrefixOrSuffixItem::Word(w) => words.push(word(w)?),
        }
    }
    if let Some(name) = &s.word_or_name {
        words.push(word(name)?);
    }
    for item in s.suffix.iter().flat_map(|p| &p.0) {
        match item {
            ast::CommandPrefixOrSuffixItem::IoRedirect(r) => redirects.extend(redirect(r)?),
            // 命令名之后的 `a=b` 只是参数（`make OUT=x`），按词处理
            ast::CommandPrefixOrSuffixItem::AssignmentWord(_, w)
            | ast::CommandPrefixOrSuffixItem::Word(w) => words.push(word(w)?),
            ast::CommandPrefixOrSuffixItem::ProcessSubstitution(..) => {
                return Err("进程替换".into());
            }
        }
    }
    Ok(SimpleCommand { words, redirects })
}

pub(crate) fn word(w: &ast::Word) -> R<Word> {
    match literal_value(&w.value) {
        Some(literal) => Ok(Word {
            raw: w.value.clone(),
            literal,
        }),
        None => Err(format!("词含展开或通配：{}", w.value)),
    }
}

/// 一个重定向；不产生文件目标的（复制文件描述符、引号界定的 here-doc）返回 `None`。
fn redirect(r: &ast::IoRedirect) -> R<Option<Redirect>> {
    use ast::IoFileRedirectKind as K;
    use ast::IoFileRedirectTarget as T;
    let fd = |fd: &Option<ast::IoFd>| -> R<Option<u32>> {
        fd.map(|n| u32::try_from(n).map_err(|_| format!("文件描述符 {n}")))
            .transpose()
    };
    match r {
        ast::IoRedirect::File(n, kind, target) => {
            let kind = match kind {
                K::Read => RedirectKind::Read,
                K::Write => RedirectKind::Write,
                K::Append => RedirectKind::Append,
                K::Clobber => RedirectKind::Clobber,
                K::ReadAndWrite => RedirectKind::ReadWrite,
                K::DuplicateInput | K::DuplicateOutput => {
                    return match target {
                        T::Fd(_) => Ok(None),
                        T::Duplicate(w) if is_fd_word(&w.value) => Ok(None),
                        _ => Err(format!("文件描述符复制到文件：{target}")),
                    };
                }
            };
            match target {
                T::Filename(w) => Ok(Some(Redirect {
                    kind,
                    fd: fd(n)?,
                    target: word(w)?,
                })),
                T::Fd(_) | T::Duplicate(_) => Err(format!("重定向目标 {target}")),
                T::ProcessSubstitution(..) => Err("进程替换".into()),
            }
        }
        ast::IoRedirect::HereDocument(_, doc) => {
            if doc.requires_expansion {
                Err("here-doc 界定符没加引号（内容会展开）".into())
            } else {
                Ok(None)
            }
        }
        ast::IoRedirect::HereString(..) => Err("here-string".into()),
        ast::IoRedirect::OutputAndError(w, append) => Ok(Some(Redirect {
            kind: if *append {
                RedirectKind::Append
            } else {
                RedirectKind::Write
            },
            fd: None,
            target: word(w)?,
        })),
    }
}

/// `>&2`、`<&-` 这类：目标是文件描述符号或 `-`，没有文件。
fn is_fd_word(raw: &str) -> bool {
    let digits = raw.strip_suffix('-').unwrap_or(raw);
    raw == "-" || (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}
