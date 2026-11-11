//! Bash 命令文字的语法解析（T07 方案 3.3、ADR-T07-a）。
//!
//! 只做一件事：把命令文字变成我们自己的小 AST [`ShellScript`]，或者说明「解析不了」。
//! 第三方解析器 `brush-parser` 只在本 crate 里出现，它的类型不外露。
//!
//! 包装层的原则：小 AST 表达不了的形态直接返回 [`Unparsable`]，不能丢掉字段后算作解析成功。
//! 小 AST 里的每个词都是字面词（[`Word::literal`]），含任何展开、通配的词都让整条解析不了；
//! 审批只看得到命令文字，文字层面确定不了的值不猜（需求第 9 章已知限制）。
//!
//! 解析不了时仍带回解析放弃前可靠识别出来的字面事实（[`PartialFacts`]）：
//! `{ printf x; } > .git/x` 虽然表达不了，字面重定向目标 `.git/x` 仍要过系统底线。

mod convert;
mod facts;
mod reparse;
mod strings;
mod word;

pub use facts::{TEXT_LIMIT, literal_facts};
pub use word::literal_value;

/// 一段 Bash 脚本：若干列表项，按出现顺序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellScript {
    pub items: Vec<ListItem>,
}

/// 一条管道及它与下一项之间的连接符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    pub pipeline: Vec<CommandNode>,
    /// `! cmd`
    pub negated: bool,
    /// 与下一项的连接；最后一项且以 `;` 或换行结尾时为 `None`
    pub connector: Option<Connector>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Connector {
    And,
    Or,
    Seq,
    Async,
}

/// 管道里的一个命令。复合命令只接受**不带重定向**的；带重定向的整条解析不了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandNode {
    Simple(SimpleCommand),
    Subshell(ShellScript),
    BraceGroup(ShellScript),
    /// if / elif / else：条件与各分支依次展平
    If {
        branches: Vec<ShellScript>,
    },
    /// while / until
    Loop {
        condition: ShellScript,
        body: ShellScript,
    },
}

/// 简单命令。前置变量赋值不在这里：出现即解析不了。`words` 可能为空（只有重定向，如 `> a.txt`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCommand {
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

/// 一个词：原文与字面值。小 AST 里只有字面词。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Word {
    pub raw: String,
    /// 去掉引号与转义之后的值（规则见 [`literal_value`]）
    pub literal: String,
}

/// 指向文件的重定向。`>&2` 这类复制文件描述符的、引号界定的 here-doc 不产生文件目标，不在这里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub kind: RedirectKind,
    pub fd: Option<u32>,
    pub target: Word,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RedirectKind {
    Read,
    Write,
    Append,
    Clobber,
    ReadWrite,
}

impl RedirectKind {
    /// 会写目标文件。
    pub fn writes(self) -> bool {
        !matches!(self, Self::Read)
    }

    /// 会读目标文件。
    pub fn reads(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }
}

/// 第一遍收到的字面事实（方案 3.3，[`literal_facts`]）。只收字面值，含展开的词不收。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialFacts {
    /// 任何节点（含复合命令、函数体）上的字面重定向目标（字面值），原文与队列里的文字都算
    pub literal_redirects: Vec<(RedirectKind, String)>,
    /// 原文里全部命令的字面词（含前置赋值词、for 的值列表），按出现顺序
    pub literal_words: Vec<String>,
    /// 工作队列里别的文字（替换体、shell 程序的参数）里的字面词：没有命令结构，只能用保守规则
    pub queued_words: Vec<String>,
    /// 处理的 shell 文字总量超过 [`TEXT_LIMIT`]，事实没收全
    pub resource_limit: bool,
    /// 整条命令原文连词法都切不出来，没有事实可抽
    pub lex_failed: bool,
}

/// 解析不了：原因给审计与卡片看，事实给归一抽路径动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unparsable {
    pub why: String,
    pub facts: PartialFacts,
}

/// 解析命令文字。纯函数：不读环境、不展开、不缓存整段解析。
///
/// 转成小 AST，遇到表达不了的形态返回 `Unparsable{why, facts}`，`facts` 是第一遍 [`literal_facts`]
/// 的结果（归类阶段放弃时调用方直接调 `literal_facts`，两种 `Unparsable` 读同一份事实）。
pub fn parse(text: &str) -> Result<ShellScript, Unparsable> {
    let fail = |why: String| Unparsable {
        why,
        facts: literal_facts(text),
    };
    if text.len() > TEXT_LIMIT {
        return Err(fail(format!("命令文字超过 {} KiB", TEXT_LIMIT / 1024)));
    }
    // 第三方解析器在异常输入上 panic 不能拖垮后台服务：按「解析不了」处理
    match std::panic::catch_unwind(|| parse_program(text)) {
        Ok(Ok(program)) => convert::program(&program).map_err(fail),
        Ok(Err(why)) => Err(fail(why)),
        Err(_) => Err(fail("解析器内部错误".into())),
    }
}

pub(crate) fn parse_program(text: &str) -> Result<brush_parser::ast::Program, String> {
    let options = brush_parser::ParserOptions::default();
    // Parser::new 走不带缓存的词法分析（cached 版本有全局状态，方案 3.3）
    let mut parser = brush_parser::Parser::new(text.as_bytes(), &options);
    parser.parse_program().map_err(|e| format!("语法错误：{e}"))
}
