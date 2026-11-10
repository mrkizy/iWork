//! 路径候选的展开：去重、全调用额度（3/F-2）。
//!
//! 解析不了的调用与白名单程序降级都要把字面词展开成许多路径候选（兜底拆词、保底角色、目录目标的
//! 「词 / 别的词的 basename」）。展开量取决于命令文字，攻击者能让它变成平方级：所以每个候选按
//! （种类, 原文, 来源, 参数标记）去重，整条调用的候选条数与候选文字字节数各有上限，**在分配之前**
//! 先扣额度，超了就失败关闭（调用方打资源上限标记，三档拒）。不截断：截断等于少检查。

use std::collections::HashSet;

use iwork_domain::action::ActionKind;

use super::paths::{fallback_candidates, is_path_word};
use super::{PathOrigin, PathQuery, PendingAction, PendingTarget};

/// 一条调用展开出的路径候选动作最多几条。
pub const MAX_EXPANSION_ITEMS: usize = 16 * 1024;
/// 一条调用展开出的候选文字最多多少字节。
pub const MAX_EXPANSION_BYTES: usize = 4 * 1024 * 1024;

/// 额度用完了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exhausted;

/// 整条调用共用的额度。
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    items: usize,
    bytes: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            items: MAX_EXPANSION_ITEMS,
            bytes: MAX_EXPANSION_BYTES,
        }
    }
}

impl Budget {
    /// 预先扣掉 `items` 条、`bytes` 字节；不够就失败，不扣。
    pub fn charge(&mut self, items: usize, bytes: usize) -> Result<(), Exhausted> {
        let items_left = self.items.checked_sub(items).ok_or(Exhausted)?;
        let bytes_left = self.bytes.checked_sub(bytes).ok_or(Exhausted)?;
        self.items = items_left;
        self.bytes = bytes_left;
        Ok(())
    }
}

type Key = (ActionKind, String, bool, bool);

/// 去重收集路径候选动作，额度与调用共用。
pub struct Expander<'a> {
    budget: &'a mut Budget,
    seen: HashSet<Key>,
    pub out: Vec<PendingAction>,
}

impl<'a> Expander<'a> {
    pub fn new(budget: &'a mut Budget) -> Self {
        Self {
            budget,
            seen: HashSet::new(),
            out: Vec::new(),
        }
    }

    /// 一个路径动作；同样的（种类, 原文, 来源, 参数标记）只出一次。
    pub fn push(
        &mut self,
        kind: ActionKind,
        literal: &str,
        origin: PathOrigin,
        argument: bool,
    ) -> Result<(), Exhausted> {
        if !is_path_word(literal) {
            return Ok(());
        }
        let key = (
            kind,
            literal.to_owned(),
            origin == PathOrigin::Real,
            argument,
        );
        if self.seen.contains(&key) {
            return Ok(());
        }
        self.budget.charge(1, literal.len())?;
        self.seen.insert(key);
        self.out.push(PendingAction {
            kind,
            target: PendingTarget::Path(PathQuery {
                literal: literal.to_owned(),
                origin,
            }),
            opaque: false,
            argument,
        });
        Ok(())
    }

    /// 一个字面词的全部兜底候选（整词 + 拆出的值，方案 3.6）。生成前先按最坏情况扣字节额度：
    /// 单个 `-` 开头的词每个切分位置都出一个后缀，总长是词长的平方级。
    pub fn candidates(&mut self, word: &str) -> Result<Vec<String>, Exhausted> {
        self.budget.charge(0, candidate_bytes_bound(word))?;
        Ok(fallback_candidates(word).into_iter().collect())
    }

    /// 额外扣条数（目录目标配对在生成前按配对数扣）。
    pub fn reserve(&mut self, items: usize, bytes: usize) -> Result<(), Exhausted> {
        self.budget.charge(items, bytes)
    }
}

/// `fallback_candidates(word)` 产出文字总长的上界（含 `~` 开头值的 `./` 变体）。
fn candidate_bytes_bound(word: &str) -> usize {
    let n = word.len();
    let splits = if word.starts_with('-') && !word.starts_with("--") {
        n.saturating_mul(n) / 2
    } else {
        n
    };
    (n + splits).saturating_mul(2).saturating_add(8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupes_and_charges_once() {
        let mut b = Budget::default();
        let mut e = Expander::new(&mut b);
        for _ in 0..3 {
            e.push(ActionKind::Delete, "x", PathOrigin::Hypothetical, false)
                .unwrap();
        }
        assert_eq!(e.out.len(), 1);
        // 对照：来源或种类不同各算一条
        e.push(ActionKind::Delete, "x", PathOrigin::Real, false)
            .unwrap();
        e.push(ActionKind::Write, "x", PathOrigin::Hypothetical, false)
            .unwrap();
        assert_eq!(e.out.len(), 3);
    }

    #[test]
    fn exhausts_before_allocating() {
        let mut b = Budget::default();
        let mut e = Expander::new(&mut b);
        // 单横线长词的拆分是平方级：先扣额度，超了直接失败
        let word = format!("-{}", "a".repeat(4096));
        assert_eq!(e.candidates(&word), Err(Exhausted));
        // 对照：普通词照常
        assert!(e.candidates("src/a.rs").is_ok());
    }
}
