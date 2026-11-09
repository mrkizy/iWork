//! 第一遍：在第三方解析器的**完整** AST 上收集字面事实（方案 3.3、第 2 轮复审 2/F-2）。
//!
//! 不管节点小 AST 支不支持都走到；只收字面值。解析放弃时这些事实随 `Unparsable` 返回，
//! 归一据此照常抽路径动作过系统底线。

use brush_parser::ast;

use crate::{PartialFacts, RedirectKind, literal_value};

pub(crate) fn collect(p: &ast::Program) -> PartialFacts {
    let mut facts = PartialFacts::default();
    for list in &p.complete_commands {
        compound_list(list, &mut facts);
    }
    facts
}

/// 第三方解析器整体失败（语法错误）时：词法分析能切出的字面词仍收下。
pub(crate) fn from_tokens(text: &str) -> PartialFacts {
    let mut facts = PartialFacts::default();
    let Ok(tokens) = brush_parser::uncached_tokenize_str(text, &Default::default()) else {
        return facts;
    };
    for token in tokens {
        if let brush_parser::Token::Word(raw, _) = token {
            push_word(&raw, &mut facts);
        }
    }
    facts
}

fn push_word(raw: &str, facts: &mut PartialFacts) {
    if let Some(literal) = literal_value(raw) {
        facts.literal_words.push(literal);
    }
}

fn compound_list(list: &ast::CompoundList, facts: &mut PartialFacts) {
    for ast::CompoundListItem(and_or, _) in &list.0 {
        pipeline(&and_or.first, facts);
        for next in &and_or.additional {
            match next {
                ast::AndOr::And(p) | ast::AndOr::Or(p) => pipeline(p, facts),
            }
        }
    }
}

fn pipeline(p: &ast::Pipeline, facts: &mut PartialFacts) {
    for c in &p.seq {
        command(c, facts);
    }
}

fn command(c: &ast::Command, facts: &mut PartialFacts) {
    match c {
        ast::Command::Simple(s) => simple(s, facts),
        ast::Command::Compound(compound, redirects) => {
            compound_command(compound, facts);
            redirect_list(redirects.as_ref(), facts);
        }
        ast::Command::Function(f) => {
            compound_command(&f.body.0, facts);
            redirect_list(f.body.1.as_ref(), facts);
        }
        ast::Command::ExtendedTest(test, redirects) => {
            extended_test(&test.expr, facts);
            redirect_list(redirects.as_ref(), facts);
        }
    }
}

fn extended_test(e: &ast::ExtendedTestExpr, facts: &mut PartialFacts) {
    use ast::ExtendedTestExpr as E;
    match e {
        E::And(a, b) | E::Or(a, b) => {
            extended_test(a, facts);
            extended_test(b, facts);
        }
        E::Not(a) | E::Parenthesized(a) => extended_test(a, facts),
        E::UnaryTest(_, w) => push_word(&w.value, facts),
        E::BinaryTest(_, a, b) => {
            push_word(&a.value, facts);
            push_word(&b.value, facts);
        }
    }
}

fn compound_command(c: &ast::CompoundCommand, facts: &mut PartialFacts) {
    use ast::CompoundCommand as C;
    match c {
        C::BraceGroup(g) => compound_list(&g.list, facts),
        C::Subshell(s) => compound_list(&s.list, facts),
        C::IfClause(i) => {
            compound_list(&i.condition, facts);
            compound_list(&i.then, facts);
            for e in i.elses.iter().flatten() {
                if let Some(cond) = &e.condition {
                    compound_list(cond, facts);
                }
                compound_list(&e.body, facts);
            }
        }
        C::WhileClause(w) | C::UntilClause(w) => {
            compound_list(&w.0, facts);
            compound_list(&w.1.list, facts);
        }
        C::ForClause(f) => {
            for w in f.values.iter().flatten() {
                push_word(&w.value, facts);
            }
            compound_list(&f.body.list, facts);
        }
        C::ArithmeticForClause(f) => compound_list(&f.body.list, facts),
        C::CaseClause(case) => {
            push_word(&case.value.value, facts);
            for item in &case.cases {
                for w in &item.patterns {
                    push_word(&w.value, facts);
                }
                if let Some(cmd) = &item.cmd {
                    compound_list(cmd, facts);
                }
            }
        }
        C::Arithmetic(_) => {}
        C::Coprocess(co) => command(&co.body, facts),
    }
}

fn simple(s: &ast::SimpleCommand, facts: &mut PartialFacts) {
    let items = s
        .prefix
        .iter()
        .flat_map(|p| &p.0)
        .chain(s.suffix.iter().flat_map(|p| &p.0));
    if let Some(name) = &s.word_or_name {
        push_word(&name.value, facts);
    }
    for item in items {
        match item {
            ast::CommandPrefixOrSuffixItem::IoRedirect(r) => redirect(r, facts),
            ast::CommandPrefixOrSuffixItem::Word(w)
            | ast::CommandPrefixOrSuffixItem::AssignmentWord(_, w) => push_word(&w.value, facts),
            ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, sub) => {
                compound_list(&sub.list, facts);
            }
        }
    }
}

fn redirect_list(list: Option<&ast::RedirectList>, facts: &mut PartialFacts) {
    for r in list.iter().flat_map(|l| &l.0) {
        redirect(r, facts);
    }
}

fn redirect(r: &ast::IoRedirect, facts: &mut PartialFacts) {
    use ast::IoFileRedirectKind as K;
    use ast::IoFileRedirectTarget as T;
    let (kind, target) = match r {
        ast::IoRedirect::File(_, kind, T::Filename(w)) => {
            let kind = match kind {
                K::Read => RedirectKind::Read,
                K::Write => RedirectKind::Write,
                K::Append => RedirectKind::Append,
                K::Clobber => RedirectKind::Clobber,
                K::ReadAndWrite => RedirectKind::ReadWrite,
                // 复制文件描述符到「文件名」（`>& file`）在 bash 里就是写这个文件
                K::DuplicateInput => RedirectKind::Read,
                K::DuplicateOutput => RedirectKind::Write,
            };
            (kind, w)
        }
        ast::IoRedirect::File(_, K::DuplicateOutput, T::Duplicate(w))
            if !w.value.chars().all(|c| c.is_ascii_digit() || c == '-') =>
        {
            (RedirectKind::Write, w)
        }
        ast::IoRedirect::File(_, _, T::ProcessSubstitution(_, sub)) => {
            compound_list(&sub.list, facts);
            return;
        }
        ast::IoRedirect::OutputAndError(w, append) => (
            if *append {
                RedirectKind::Append
            } else {
                RedirectKind::Write
            },
            w,
        ),
        ast::IoRedirect::HereString(_, w) => {
            push_word(&w.value, facts);
            return;
        }
        ast::IoRedirect::File(..) | ast::IoRedirect::HereDocument(..) => return,
    };
    if let Some(literal) = literal_value(&target.value) {
        facts.literal_redirects.push((kind, literal));
    }
}
