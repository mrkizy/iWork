//! 第一遍：收集字面事实（方案 3.3）。`parse` 放弃、归类阶段放弃两种 `Unparsable` 都读这一份。
//!
//! 用一个显式工作队列迭代，不递归，所以**没有深度上限**：原文先入队；处理一段文字时，
//! 它里面的命令替换体（任何词里都收，见 [`Walk::substitutions`]）和登记表里 shell 程序的字符串参数
//! （见 [`crate::reparse`]）再入队，按同一套办法处理。唯一的资源上限是处理过的文字总量
//! [`TEXT_LIMIT`]：命中时立即停止并置 `resource_limit`，不能退成「空事实 + 普通询问」。
//!
//! 一段文字的处理：第三方解析器能出 AST 时走完整 AST；整体语法错时退到词法分析切出的词；
//! 词法也失败时，原文置 `lex_failed`（没有事实可抽，归一据此拒绝），队列里的文字按空白与元字符粗切。

use std::collections::{HashSet, VecDeque};

use brush_parser::ast;
use brush_parser::word::{WordPiece, WordPieceWithSource};

use crate::reparse::texts_to_reparse;
use crate::{PartialFacts, RedirectKind, literal_value, strings};

/// 处理过的 shell 文字总字节数上限（原文 + 全部入队文字）。
pub const TEXT_LIMIT: usize = 256 * 1024;

/// 第一遍的统一入口（方案 3.3 `literal_facts`）。纯函数。
pub fn literal_facts(text: &str) -> PartialFacts {
    let mut walk = Walk::default();
    walk.enqueue(text.to_owned(), Origin::Original);
    let mut processed = 0usize;
    while let Some((t, origin)) = walk.queue.pop_front() {
        processed = processed.saturating_add(t.len());
        if processed > TEXT_LIMIT {
            walk.facts.resource_limit = true;
            break;
        }
        walk.text(&t, origin);
        if walk.facts.lex_failed {
            break;
        }
    }
    walk.facts
}

/// 一段文字从哪来：原文的词进 `literal_words`；队列里收来的词没有命令结构，进 `queued_words`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Original,
    Queued,
}

#[derive(Default)]
struct Walk {
    facts: PartialFacts,
    queue: VecDeque<(String, Origin)>,
    /// 进过队列的文字按内容去重，只处理一次，所以不会绕圈
    seen: HashSet<String>,
}

impl Walk {
    fn enqueue(&mut self, text: String, origin: Origin) {
        if !text.trim().is_empty() && self.seen.insert(text.clone()) {
            self.queue.push_back((text, origin));
        }
    }

    fn text(&mut self, text: &str, origin: Origin) {
        let parsed = std::panic::catch_unwind(|| crate::parse_program(text));
        if let Ok(Ok(program)) = parsed {
            let mut ast = AstWalk { walk: self, origin };
            for list in &program.complete_commands {
                ast.compound_list(list);
            }
            if has_expansion_marker(text) {
                for leaf in strings::leaves(&program) {
                    self.substitutions(&leaf);
                }
            }
            return;
        }
        // 整体语法错：bash 遇到后面某行语法错时前面完整的行已经执行了，词法切出的词照样要抽
        let tokens = std::panic::catch_unwind(|| {
            brush_parser::uncached_tokenize_str(text, &Default::default())
        });
        match tokens {
            Ok(Ok(tokens)) => {
                let raws: Vec<String> = tokens
                    .into_iter()
                    .filter_map(|t| match t {
                        brush_parser::Token::Word(raw, _) => Some(raw),
                        brush_parser::Token::Operator(..) => None,
                    })
                    .collect();
                // 没有命令边界：再解析登记表取到末尾
                let literals: Vec<String> = raws.iter().filter_map(|r| literal_value(r)).collect();
                self.words(&literals, origin);
                for raw in &raws {
                    self.substitutions(raw);
                }
            }
            _ if origin == Origin::Original => self.facts.lex_failed = true,
            // 队列里的文字词法失败不拒：替换体有一部分是从不展开的位置多收来的普通文字。粗切，尽力而为
            _ => self.facts.queued_words.extend(coarse_words(text)),
        }
    }

    /// 一条命令（或没有命令边界时的一串）字面词：记下，并按登记表把 shell 程序的参数入队。
    fn words(&mut self, literals: &[String], origin: Origin) {
        let target = match origin {
            Origin::Original => &mut self.facts.literal_words,
            Origin::Queued => &mut self.facts.queued_words,
        };
        target.extend(literals.iter().cloned());
        match texts_to_reparse(literals) {
            Ok(texts) => {
                for t in texts {
                    self.enqueue(t, Origin::Queued);
                }
            }
            Err(_) => self.facts.resource_limit = true,
        }
    }

    /// 一个字符串叶子里的命令替换体入队（方案 3.3 第 2 条规则）。
    ///
    /// 用解析器自己的词语法拆片段：命令替换与反引号片段的正文入队；片段里剩下的字符串叶子
    /// （如 `${x:-…}` 的默认值、`$((…))` 的表达式原文）比原叶子短的再拆一次（只收更短的，保证会停）。
    /// 词和 here-doc 正文的引号规则不同，两种读法都跑、取并集；都解析失败的叶子整段当 shell 文字入队。
    fn substitutions(&mut self, leaf: &str) {
        let mut stack = vec![leaf.to_owned()];
        let mut done: HashSet<String> = HashSet::new();
        while let Some(l) = stack.pop() {
            if !has_expansion_marker(&l) || !done.insert(l.clone()) {
                continue;
            }
            let options = brush_parser::ParserOptions::default();
            let readings = [
                std::panic::catch_unwind(|| brush_parser::word::parse(&l, &options)),
                std::panic::catch_unwind(|| brush_parser::word::parse_heredoc(&l, &options)),
            ];
            let mut any = false;
            for pieces in readings.into_iter().filter_map(|r| r.ok()?.ok()) {
                any = true;
                self.pieces(&pieces, l.len(), &mut stack);
            }
            if !any {
                self.enqueue(l, Origin::Queued);
            }
        }
    }

    fn pieces(&mut self, pieces: &[WordPieceWithSource], len: usize, stack: &mut Vec<String>) {
        for p in pieces {
            match &p.piece {
                WordPiece::CommandSubstitution(body)
                | WordPiece::BackquotedCommandSubstitution(body) => {
                    self.enqueue(body.clone(), Origin::Queued);
                }
                WordPiece::DoubleQuotedSequence(inner)
                | WordPiece::GettextDoubleQuotedSequence(inner) => self.pieces(inner, len, stack),
                other => stack.extend(strings::leaves(other).into_iter().filter(|s| s.len() < len)),
            }
        }
    }
}

/// 只有含 `$` 或反引号的文字才可能有命令替换：热路径上多数命令省掉整棵 AST 的遍历。
fn has_expansion_marker(text: &str) -> bool {
    text.contains(['$', '`'])
}

/// 按空白与 `; & | < > ( )` 切开，每段去掉引号与反斜杠。这种切法不会失败。
fn coarse_words(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || ";&|<>()".contains(c))
        .map(|s| {
            s.chars()
                .filter(|c| !matches!(c, '\'' | '"' | '\\'))
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// 完整 AST 上收字面重定向与每条简单命令的字面词。不管节点小 AST 支不支持都走到。
struct AstWalk<'a> {
    walk: &'a mut Walk,
    origin: Origin,
}

impl AstWalk<'_> {
    fn compound_list(&mut self, list: &ast::CompoundList) {
        for ast::CompoundListItem(and_or, _) in &list.0 {
            self.pipeline(&and_or.first);
            for next in &and_or.additional {
                match next {
                    ast::AndOr::And(p) | ast::AndOr::Or(p) => self.pipeline(p),
                }
            }
        }
    }

    fn pipeline(&mut self, p: &ast::Pipeline) {
        for c in &p.seq {
            self.command(c);
        }
    }

    fn command(&mut self, c: &ast::Command) {
        match c {
            ast::Command::Simple(s) => self.simple(s),
            ast::Command::Compound(compound, redirects) => {
                self.compound_command(compound);
                self.redirect_list(redirects.as_ref());
            }
            ast::Command::Function(f) => {
                self.compound_command(&f.body.0);
                self.redirect_list(f.body.1.as_ref());
            }
            ast::Command::ExtendedTest(test, redirects) => {
                let mut words = Vec::new();
                extended_test(&test.expr, &mut words);
                self.walk.words(&literals(&words), self.origin);
                self.redirect_list(redirects.as_ref());
            }
        }
    }

    fn compound_command(&mut self, c: &ast::CompoundCommand) {
        use ast::CompoundCommand as C;
        match c {
            C::BraceGroup(g) => self.compound_list(&g.list),
            C::Subshell(s) => self.compound_list(&s.list),
            C::IfClause(i) => {
                self.compound_list(&i.condition);
                self.compound_list(&i.then);
                for e in i.elses.iter().flatten() {
                    if let Some(cond) = &e.condition {
                        self.compound_list(cond);
                    }
                    self.compound_list(&e.body);
                }
            }
            C::WhileClause(w) | C::UntilClause(w) => {
                self.compound_list(&w.0);
                self.compound_list(&w.1.list);
            }
            C::ForClause(f) => {
                let words: Vec<&str> = f
                    .values
                    .iter()
                    .flatten()
                    .map(|w| w.value.as_str())
                    .collect();
                self.walk.words(&literals(&words), self.origin);
                self.compound_list(&f.body.list);
            }
            C::ArithmeticForClause(f) => self.compound_list(&f.body.list),
            C::CaseClause(case) => {
                let mut words = vec![case.value.value.as_str()];
                words.extend(
                    case.cases
                        .iter()
                        .flat_map(|i| &i.patterns)
                        .map(|w| w.value.as_str()),
                );
                self.walk.words(&literals(&words), self.origin);
                for item in &case.cases {
                    if let Some(cmd) = &item.cmd {
                        self.compound_list(cmd);
                    }
                }
            }
            C::Arithmetic(_) => {}
            C::Coprocess(co) => self.command(&co.body),
        }
    }

    /// 词按原文顺序（前置部分、命令名、后置部分）收，再解析登记表取到这条命令结束。
    fn simple(&mut self, s: &ast::SimpleCommand) {
        let mut words: Vec<&str> = Vec::new();
        for it in s.prefix.iter().flat_map(|p| &p.0) {
            words.extend(self.item(it));
        }
        if let Some(name) = &s.word_or_name {
            words.push(name.value.as_str());
        }
        for it in s.suffix.iter().flat_map(|p| &p.0) {
            words.extend(self.item(it));
        }
        self.walk.words(&literals(&words), self.origin);
    }

    /// 前置或后置的一项：词返回原文；重定向、进程替换当场收。
    fn item<'s>(&mut self, it: &'s ast::CommandPrefixOrSuffixItem) -> Option<&'s str> {
        match it {
            ast::CommandPrefixOrSuffixItem::IoRedirect(r) => self.redirect(r),
            ast::CommandPrefixOrSuffixItem::Word(w)
            | ast::CommandPrefixOrSuffixItem::AssignmentWord(_, w) => return Some(&w.value),
            ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, sub) => {
                self.compound_list(&sub.list);
            }
        }
        None
    }

    fn redirect_list(&mut self, list: Option<&ast::RedirectList>) {
        for r in list.iter().flat_map(|l| &l.0) {
            self.redirect(r);
        }
    }

    fn redirect(&mut self, r: &ast::IoRedirect) {
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
                self.compound_list(&sub.list);
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
                self.walk.words(&literals(&[w.value.as_str()]), self.origin);
                return;
            }
            ast::IoRedirect::File(..) | ast::IoRedirect::HereDocument(..) => return,
        };
        if let Some(literal) = literal_value(&target.value) {
            self.walk.facts.literal_redirects.push((kind, literal));
        }
    }
}

fn extended_test<'a>(e: &'a ast::ExtendedTestExpr, out: &mut Vec<&'a str>) {
    use ast::ExtendedTestExpr as E;
    match e {
        E::And(a, b) | E::Or(a, b) => {
            extended_test(a, out);
            extended_test(b, out);
        }
        E::Not(a) | E::Parenthesized(a) => extended_test(a, out),
        E::UnaryTest(_, w) => out.push(&w.value),
        E::BinaryTest(_, a, b) => {
            out.push(&a.value);
            out.push(&b.value);
        }
    }
}

/// 只收字面词；含展开的词本身不收（它里面的替换体由通用遍历收）。
fn literals<S: AsRef<str>>(raws: &[S]) -> Vec<String> {
    raws.iter()
        .filter_map(|r| literal_value(r.as_ref()))
        .collect()
}
