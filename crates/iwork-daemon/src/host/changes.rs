//! 改动探针（T10a 方案 D10、2.4）：回合开始前拍一份「相对基线有改动的文件此刻的版本」，收尾时再拍一份，
//! 两份里版本不一样的就是这一趟期间变了的。版本由版本控制端口给出，后台自己不按工作区的路径读文件（6/F-1）。
//!
//! 读有上限（4/F-3）：全后台同时最多 [`READS_IN_FLIGHT`] 个读在跑；额度由那次读自己拿着，**它真的结束才还**，
//! 等的人到时放手不还。额度满了新来的不排队、不等，这一趟直接没有摘要。
//!
//! **7/F-2 修好之前探针不启用**：组装处给的是 `enabled = false` 的那一个，`capture` 一次版本控制调用都不发。

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use iwork_domain::event::ChangeSummary;
use iwork_ports::vcs::{BaselineRef, ChangeProbe, DiffRange, PathVersion, RepoRoot, VcsProvider};

/// 全后台同时在跑的拍改动的读最多几个。
pub const READS_IN_FLIGHT: usize = 2;
/// 拍开始那一份最多等多久；到时这一趟不带探针。
pub const CAPTURE_LIMIT: Duration = Duration::from_secs(5);

/// 会话线任务拍开始那一份用的接缝。
pub trait ChangeReader: Send + Sync + 'static {
    /// 拍开始那一份。关着、拿不到额度、拍不出来、到时 → `None`（这一趟不出改动摘要，照常往下）。
    fn capture(
        &self,
        workspace: &Path,
    ) -> impl Future<Output = Option<Arc<dyn ChangeProbe>>> + Send;
}

/// 读数额度：一个计数，拿不到就算了，不等。
#[derive(Debug, Default)]
pub struct ReadBudget(AtomicUsize);

/// 拿着的一个额度；丢掉即还。跟着那次读走，不跟着等的人。
pub struct Permit(Arc<ReadBudget>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ReadBudget {
    pub fn try_take(self: &Arc<Self>) -> Option<Permit> {
        self.0
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < READS_IN_FLIGHT).then_some(n + 1)
            })
            .ok()
            .map(|_| Permit(self.clone()))
    }

    pub fn in_flight(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

/// 一次读交给不随等的人被丢掉的小任务：额度跟着它，它结束才还；等的人最多等 `limit`。
/// 读 panic、出错、到时都只算「没有」。
async fn bounded_read<T, F>(permit: Permit, limit: Duration, read: F) -> Option<T>
where
    T: Send + 'static,
    F: Future<Output = Option<T>> + Send + 'static,
{
    let task = tokio::spawn(async move {
        let out = read.await;
        drop(permit);
        out
    });
    match tokio::time::timeout(limit, task).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "拍改动的读异常结束，这一趟不带改动摘要");
            None
        }
        Err(_) => {
            tracing::warn!(?limit, "拍改动的读到时没回来，这一趟不带改动摘要");
            None
        }
    }
}

/// 经版本控制端口拍改动。
pub struct VcsChanges<V> {
    vcs: Arc<V>,
    enabled: bool,
    budget: Arc<ReadBudget>,
    capture_limit: Duration,
}

impl<V> VcsChanges<V> {
    /// `enabled = false`：探针关着，一次版本控制调用都不发（7/F-2 修好之前组装处这样给）。
    pub fn new(vcs: Arc<V>, enabled: bool, budget: Arc<ReadBudget>) -> Self {
        Self {
            vcs,
            enabled,
            budget,
            capture_limit: CAPTURE_LIMIT,
        }
    }

    /// 测试用：拍开始那一份的时限换成更短的（真时钟上跑「读一直不返回」的格子）。
    pub fn with_capture_limit(mut self, limit: Duration) -> Self {
        self.capture_limit = limit;
        self
    }
}

/// 开始那一份。
struct Captured {
    baseline: BaselineRef,
    files: BTreeMap<PathBuf, PathVersion>,
}

async fn versions_of<V: VcsProvider>(
    vcs: &V,
    repo: &RepoRoot,
    paths: Vec<PathBuf>,
) -> Option<BTreeMap<PathBuf, PathVersion>> {
    let versions = vcs.worktree_versions(repo, &paths).await.ok()?;
    (versions.len() == paths.len()).then(|| paths.into_iter().zip(versions).collect())
}

impl<V: VcsProvider + 'static> ChangeReader for VcsChanges<V> {
    async fn capture(&self, workspace: &Path) -> Option<Arc<dyn ChangeProbe>> {
        if !self.enabled {
            return None;
        }
        let permit = self.budget.try_take()?;
        let (vcs, repo) = (self.vcs.clone(), RepoRoot(workspace.to_path_buf()));
        let read_repo = repo.clone();
        let captured = bounded_read(permit, self.capture_limit, async move {
            let baseline = vcs.baseline(&read_repo).await.ok()?;
            let status = vcs.status(&read_repo).await.ok()?;
            let paths = status.changed.into_iter().map(|c| c.path).collect();
            let files = versions_of(vcs.as_ref(), &read_repo, paths).await?;
            Some(Captured { baseline, files })
        })
        .await?;
        Some(Arc::new(Probe {
            vcs: self.vcs.clone(),
            repo,
            start: Arc::new(captured),
            budget: self.budget.clone(),
        }))
    }
}

/// 一趟自己的探针。
struct Probe<V> {
    vcs: Arc<V>,
    repo: RepoRoot,
    start: Arc<Captured>,
    budget: Arc<ReadBudget>,
}

/// 这一趟期间变了的路径（纯函数）。`now` 是此刻要看的路径各自的版本：开始那一份里的、此刻相对基线有改动的、
/// 基线挪过时两条基线之间变了的。开始那一份里没有的路径，开始时和基线一样，现在出现了就是变了；有的，
/// 版本不同或有一头说不清就是变了。
pub fn changed_paths(
    start: &BTreeMap<PathBuf, PathVersion>,
    now: &BTreeMap<PathBuf, PathVersion>,
) -> Vec<PathBuf> {
    now.iter()
        .filter(|(path, v)| match start.get(*path) {
            None => true,
            Some(before) => {
                before != *v
                    || matches!(before, PathVersion::Unknown)
                    || matches!(v, PathVersion::Unknown)
            }
        })
        .map(|(p, _)| p.clone())
        .collect()
}

impl<V: VcsProvider + 'static> ChangeProbe for Probe<V> {
    fn summary(&self) -> Pin<Box<dyn Future<Output = Option<ChangeSummary>> + Send + '_>> {
        Box::pin(async move {
            let permit = self.budget.try_take()?;
            let (vcs, repo, start) = (self.vcs.clone(), self.repo.clone(), self.start.clone());
            // 收尾那一份的时限由编排管（CHANGE_SUMMARY_LIMIT）；这里给一个更长的保底，读本身照样占着额度直到结束
            bounded_read(permit, Duration::from_secs(3600), async move {
                let mut paths: BTreeSet<PathBuf> = start.files.keys().cloned().collect();
                let status = vcs.status(&repo).await.ok()?;
                paths.extend(status.changed.into_iter().map(|c| c.path));
                let now_base = vcs.baseline(&repo).await.ok()?;
                if now_base != start.baseline {
                    let moved = vcs
                        .diff(
                            &repo,
                            DiffRange::Between {
                                from: start.baseline.clone(),
                                to: now_base,
                            },
                        )
                        .await
                        .ok()?;
                    paths.extend(moved.files.into_iter().map(|f| f.path));
                }
                let now = versions_of(vcs.as_ref(), &repo, paths.into_iter().collect()).await?;
                let changed = changed_paths(&start.files, &now);
                // 行数是给人看的数，取不到记 0，不影响盖不盖章
                let lines = vcs
                    .diff(&repo, DiffRange::WorkspaceSince(start.baseline.clone()))
                    .await
                    .map(|d| {
                        d.files
                            .into_iter()
                            .filter(|f| changed.contains(&f.path))
                            .fold((0u32, 0u32), |(i, d), f| {
                                (
                                    i.saturating_add(f.insertions),
                                    d.saturating_add(f.deletions),
                                )
                            })
                    })
                    .unwrap_or((0, 0));
                Some(ChangeSummary {
                    files_changed: u32::try_from(changed.len()).unwrap_or(u32::MAX),
                    insertions: lines.0,
                    deletions: lines.1,
                    paths: changed,
                })
            })
            .await
        })
    }
}

#[cfg(test)]
#[path = "changes_tests.rs"]
mod tests;
