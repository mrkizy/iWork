//! 路径候选的展开：去重、全调用额度（3/F-2）。
//!
//! Bash 调用的**全部**动作都经这里产出（5/F-1 的根因修法）：路径动作按（种类, 原文, 来源, 参数标记）
//! 去重、扣额度；非路径动作（执行、推送、发布…）只扣条数。展开量取决于命令文字，攻击者能让它变成
//! 平方级（兜底拆词、保底角色、目录目标的「目录 / 文件名」、`find` 的删除），所以整条调用的条数与
//! 文字字节数各有上限，**在生成、拼接之前**先扣，超了就失败关闭（调用方打资源上限标记，三档拒）。
//! 不截断：截断等于少检查。新增候选来源时只能调这里的方法，`bash.rs` 不直接构造路径动作。

use std::collections::HashSet;

use iwork_domain::action::ActionKind;

use super::paths::{basename, fallback_candidates, is_path_word, join_dir};
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

    /// 一个不是路径的动作（执行、推送、发布、承载命令的动作）：不去重，只扣一条。
    pub fn push_action(&mut self, action: PendingAction) -> Result<(), Exhausted> {
        debug_assert!(
            !matches!(action.target, PendingTarget::Path(_)),
            "路径动作必须经 push 去重扣额度"
        );
        self.budget.charge(1, 0)?;
        self.out.push(action);
        Ok(())
    }

    /// 同一份额度、独立去重与输出的子收集器：先试着归类（白名单安全形态），成了再并回来。
    pub fn child(&mut self) -> Expander<'_> {
        Expander::new(self.budget)
    }

    /// 把子收集器的结果并回来（额度已扣过）。
    pub fn adopt(&mut self, actions: Vec<PendingAction>) {
        for a in &actions {
            if let PendingTarget::Path(q) = &a.target {
                self.seen.insert((
                    a.kind,
                    q.literal.clone(),
                    q.origin == PathOrigin::Real,
                    a.argument,
                ));
            }
        }
        self.out.extend(actions);
    }

    /// 目录目标：每个目的 `d` 与每个**唯一**的文件名配出写入候选 `d / 文件名`（按 `roles`）。
    /// 配对数是平方级：按唯一（目的, 文件名）组合生成，拼接之前先按上界扣字节额度（5/F-1、4/F-1）。
    /// `exclude_self` 时只有 `d` 自己给出的文件名不配（「别的候选」语义）。
    pub fn dir_pairs(
        &mut self,
        roles: &[ActionKind],
        dests: &[&str],
        sources: &[&str],
        exclude_self: bool,
    ) -> Result<(), Exhausted> {
        let dests: std::collections::BTreeSet<&str> =
            dests.iter().copied().filter(|d| is_path_word(d)).collect();
        let mut bases: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for s in sources {
            if let Some(base) = basename(s) {
                *bases.entry(base).or_default() += 1;
            }
        }
        if dests.is_empty() || bases.is_empty() {
            return Ok(());
        }
        let dest_bytes: usize = dests.iter().map(|d| d.len() + 1).sum();
        let base_bytes: usize = bases.keys().map(|b| b.len()).sum();
        let bound = bases
            .len()
            .saturating_mul(dest_bytes)
            .saturating_add(dests.len().saturating_mul(base_bytes));
        self.budget.charge(0, bound)?;
        for d in &dests {
            let own = basename(d);
            for (base, count) in &bases {
                if exclude_self && own == Some(*base) && *count == 1 {
                    continue;
                }
                let joined = join_dir(d, base);
                for kind in roles {
                    self.push(*kind, &joined, PathOrigin::Hypothetical, false)?;
                }
            }
        }
        Ok(())
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
