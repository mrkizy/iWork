//! 决策令牌、屏障与追平（T07 方案 6.10.3、6.10.4），以及清孤儿（6.10.6 第 6 条）。
//!
//! 令牌不保护数据，只让同一个对话里「追平 → 计划 → 提交」排队；折叠只认日志。
//! 屏障：持有者在等提交回执时被取消，令牌放了、那笔还在写队列里。令牌记着「有一笔去向不明」，
//! 下一位先排一笔空提交，等它回来（写线程按入队顺序处理，那笔一定有了结果）再读。
use super::fold::FoldError;
use super::lock::OwnedAsyncGuard;
use super::*;
use iwork_ports::store::{EventFilter, StreamSelector};

/// 一次追平最多读多少条，读满了接着读。
pub const CATCH_UP_BATCH: usize = 512;
/// 提交被投影器拒（状态前提不成立）后追平重算，最多这么多次。
pub const REPLAN_ATTEMPTS: u32 = 3;

/// 每个对话一把。锁里只放屏障要用的一个标记。
#[derive(Default)]
pub(super) struct Token {
    unsettled: bool,
}

pub(super) type TokenGuard = OwnedAsyncGuard<Token>;

/// 作废计划交出期间占着的令牌（T05 接缝的 `VoidHold`）。丢弃即释放；创建时就记成「有一笔去向不明」：
/// 引擎看不到编排那笔提交回没回来，下一位一律先过屏障。
pub struct VoidHold {
    _token: TokenGuard,
}

/// 引擎读不动、折不下去，或重算 3 次仍冲突。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Fold(#[from] FoldError),
    #[error("提交在追平后重算 {REPLAN_ATTEMPTS} 次仍被拒")]
    Conflict,
}

fn engine_filter() -> EventFilter {
    EventFilter::Streams(
        fold::ENGINE_STREAMS
            .iter()
            .map(|kind| StreamSelector {
                kind: *kind,
                id: None,
            })
            .collect(),
    )
}

impl<C, G, S, A, T, R> ApprovalEngine<C, G, S, A, T, R>
where
    C: Clock + 'static,
    G: IdGenerator + 'static,
    S: StoragePort + 'static,
    A: AgentPort + 'static,
    T: Timer + 'static,
    R: PathResolver + 'static,
{
    fn token(&self, conversation: ConversationId) -> Arc<AsyncLock<Token>> {
        lock_std(&self.tokens)
            .entry(conversation)
            .or_insert_with(|| Arc::new(AsyncLock::new(Token::default())))
            .clone()
    }

    /// 拿令牌，标记为真时先过屏障。屏障失败（`Busy`、`Io`）标记留着，本次按失败处理，下一位再做一遍。
    pub(super) async fn acquire(
        &self,
        conversation: ConversationId,
    ) -> Result<TokenGuard, StorageError> {
        let mut token = self.token(conversation).lock_owned().await;
        if token.unsettled {
            self.ports
                .store
                .commit(self.request(ENGINE_ACTOR, Vec::new()))
                .await?;
            token.unsettled = false;
        }
        Ok(token)
    }

    /// 在令牌下提交。等回执时被取消，标记留着；回来了（成功失败都算）就清掉。
    pub(super) async fn commit_under(
        &self,
        tokens: &mut [&mut TokenGuard],
        actor: &str,
        events: Vec<DomainEvent>,
    ) -> Result<CommitReceipt, StorageError> {
        let req = self.request(actor, events);
        self.commit_request(tokens, req).await
    }

    pub(super) async fn commit_request(
        &self,
        tokens: &mut [&mut TokenGuard],
        req: CommitRequest,
    ) -> Result<CommitReceipt, StorageError> {
        for t in tokens.iter_mut() {
            t.unsettled = true;
        }
        let result = self.ports.store.commit(req).await;
        for t in tokens.iter_mut() {
            t.unsettled = false;
        }
        result
    }

    /// 交给编排的作废句柄：令牌带着出去，记成去向不明。
    pub(super) fn hold(mut token: TokenGuard) -> VoidHold {
        token.unsettled = true;
        VoidHold { _token: token }
    }

    /// 把折叠追到存储当前的 head。可以并发调，可以随时丢弃：游标只在应用完一条事件后前进。
    /// 读失败或折叠报错时游标停在出错的那条之前，调用它的决定按失败处理。
    pub async fn catch_up(&self) -> Result<(), EngineError> {
        loop {
            let from = lock_std(&self.inner).folded.cursor();
            let batch = self
                .ports
                .store
                .read_events(engine_filter(), from, CATCH_UP_BATCH)
                .await?;
            let n = batch.len();
            {
                let mut inner = lock_std(&self.inner);
                for e in &batch {
                    if e.seq <= inner.folded.cursor() {
                        continue; // 别的任务已经应用过
                    }
                    let effects = inner.folded.apply(e.seq, &e.event)?;
                    inner.run(effects);
                }
            }
            if n < CATCH_UP_BATCH {
                return Ok(());
            }
        }
    }

    /// 已经提交成功之后的追平：失败不改提交的结果，记一条错误日志；下一个决定自己追平时会再撞上它。
    pub(super) async fn catch_up_after_commit(&self, path: &'static str) -> bool {
        match self.catch_up().await {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(path, %error, cursor = lock_std(&self.inner).folded.cursor().0, "审批引擎提交后追平失败");
                false
            }
        }
    }

    /// 给 daemon 的入口：追平，然后清孤儿。会拿令牌、会提交，只能在写线程之外、手里没有令牌的地方调。
    pub async fn sync(&self) -> Result<(), EngineError> {
        self.catch_up().await?;
        self.sweep_orphans().await
    }

    /// T05 的 `committed` 回调用这个：只追平、不提交。失败在这里记 error 日志（回调没有返回值）。
    pub async fn nudge(&self) {
        if let Err(error) = self.catch_up().await {
            tracing::error!(%error, cursor = lock_std(&self.inner).folded.cursor().0, "审批引擎追平失败");
        }
    }

    /// 做常设义务：孤儿作废、失去执行者的审查放弃（`Inner::due`）。谁调都一样，调几次都一样；
    /// 中途被丢弃，剩下的还在，下一位接着做。前提：手里没有任何令牌、不在存储的写线程上。
    /// 一个对话失败不挡别的对话，返回第一个错误。
    pub async fn sweep_orphans(&self) -> Result<(), EngineError> {
        let conversations = lock_std(&self.inner).due_conversations();
        let mut first_error = None;
        for conversation in conversations {
            if let Err(error) = self.sweep_conversation(conversation).await {
                tracing::error!(%conversation, %error, "清孤儿 / 放弃无人执行的审查失败");
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn sweep_conversation(&self, conversation: ConversationId) -> Result<(), EngineError> {
        for _ in 0..REPLAN_ATTEMPTS {
            let mut token = self.acquire(conversation).await?;
            self.catch_up().await?;
            let events = lock_std(&self.inner).due(conversation);
            if events.is_empty() {
                return Ok(());
            }
            match self
                .commit_under(&mut [&mut token], ENGINE_ACTOR, events)
                .await
            {
                Ok(_) => {
                    self.catch_up_after_commit("sweep_orphans").await;
                    return Ok(());
                }
                Err(StorageError::Projection { .. }) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(EngineError::Conflict)
    }
}
