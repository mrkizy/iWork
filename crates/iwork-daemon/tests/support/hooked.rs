//! 回合宿主测试用的存储与读模型外壳：照原样转给里面那一个，另外能让指定的读或提交停住、数调用次数。
//! 停住的位置在带闸存储的**外面**：被放开之后入不入得了队，由真的闸决定（E4）。

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use iwork_domain::common::ArtifactRef;
use iwork_domain::id::{CommandId, ConversationId, MessageId, ProjectId, Seq, TurnId};
use iwork_ports::read_model::*;
use iwork_ports::store::*;
use tokio::sync::Notify;

/// 一处可以停住的调用：`calls` 数全部调用；装上（`arm`）之后进来的调用停在这里，直到 `release`。
#[derive(Default)]
pub struct Stall {
    armed: AtomicBool,
    pub calls: AtomicUsize,
    /// 正停在这里的调用数
    pub stuck: AtomicUsize,
    entered: Notify,
    release: Notify,
}

impl Stall {
    pub fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// 放开停着的，之后进来的也不再停。
    pub fn release(&self) {
        self.armed.store(false, Ordering::SeqCst);
        self.release.notify_waiters();
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// 等到至少有一个调用停在这里。
    pub async fn wait_stuck(&self) {
        loop {
            let entered = self.entered.notified();
            if self.stuck.load(Ordering::SeqCst) > 0 {
                return;
            }
            entered.await;
        }
    }

    async fn pass(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.armed.load(Ordering::SeqCst) {
            return;
        }
        let go = self.release.notified();
        tokio::pin!(go);
        go.as_mut().enable();
        if !self.armed.load(Ordering::SeqCst) {
            return;
        }
        self.stuck.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_waiters();
        go.await;
        self.stuck.fetch_sub(1, Ordering::SeqCst);
    }
}

type CommitPick = Arc<dyn Fn(&CommitRequest) -> bool + Send + Sync>;

/// 提交外壳的开关：只有 `pick` 选中的提交受 `stall` 管、被 `picked` 数到；没装 `pick` 时一笔都不选。
#[derive(Default)]
pub struct CommitKnob {
    pick: Mutex<Option<CommitPick>>,
    pub stall: Stall,
    /// 被选中、并且从外壳往里交了的提交数（停住的那一笔被放开之后才算）
    pub passed_in: AtomicUsize,
}

impl CommitKnob {
    pub fn select(&self, f: impl Fn(&CommitRequest) -> bool + Send + Sync + 'static) {
        *self.pick.lock().unwrap() = Some(Arc::new(f));
    }

    fn picked(&self, req: &CommitRequest) -> bool {
        self.pick.lock().unwrap().as_ref().is_some_and(|f| f(req))
    }
}

/// 全部开关。一套夹具一份，测试经 `H::knobs` 拿到。
#[derive(Default)]
pub struct Knobs {
    pub commit: CommitKnob,
    /// 存储的 `read_events`（引擎追平读的就是它）
    pub read_events: Stall,
    /// 读模型：宿主对账的读
    pub turn_work: Stall,
    /// 读模型：会话线准备的读（第一项）
    pub turn_with_conversation: Stall,
    /// 读模型：换线时取最后一条会话绑定
    pub last_session_binding: Stall,
    /// 读模型：退出前数目
    pub quit_counts: Stall,
    /// 读模型：补账前读流式消息
    pub unsettled: Stall,
}

pub struct HookedStore<S> {
    pub inner: S,
    pub knobs: Arc<Knobs>,
}

impl<S: Clone> Clone for HookedStore<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            knobs: self.knobs.clone(),
        }
    }
}

impl<S: StoragePort> StoragePort for HookedStore<S> {
    async fn commit(&self, req: CommitRequest) -> Result<CommitReceipt, StorageError> {
        let k = &self.knobs.commit;
        if k.picked(&req) {
            k.stall.pass().await;
            k.passed_in.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.commit(req).await
    }
    async fn command_receipt(&self, id: CommandId) -> Result<Option<CommitReceipt>, StorageError> {
        self.inner.command_receipt(id).await
    }
    async fn head_seq(&self) -> Result<Seq, StorageError> {
        self.inner.head_seq().await
    }
    async fn read_events(
        &self,
        filter: EventFilter,
        from: Seq,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        self.knobs.read_events.pass().await;
        self.inner.read_events(filter, from, limit).await
    }
    async fn checkpoint_message(
        &self,
        message_id: MessageId,
        offset: u64,
        text: &str,
    ) -> Result<(), StorageError> {
        self.inner
            .checkpoint_message(message_id, offset, text)
            .await
    }
    async fn outbox_pending(&self, limit: usize) -> Result<Vec<OutboxItem>, StorageError> {
        self.inner.outbox_pending(limit).await
    }
    async fn outbox_mark(&self, id: OutboxId, state: DeliveryState) -> Result<(), StorageError> {
        self.inner.outbox_mark(id, state).await
    }
    async fn outbox_requeue_dispatched(
        &self,
        redeliver_prefixes: Vec<String>,
        fail_prefixes: Vec<String>,
    ) -> Result<RequeueReport, StorageError> {
        self.inner
            .outbox_requeue_dispatched(redeliver_prefixes, fail_prefixes)
            .await
    }
    async fn outbox_pending_where(
        &self,
        selector: OutboxSelector,
        limit: usize,
    ) -> Result<Vec<OutboxItem>, StorageError> {
        self.inner.outbox_pending_where(selector, limit).await
    }
    async fn rebuild_projections(&self) -> Result<RebuildReport, StorageError> {
        self.inner.rebuild_projections().await
    }
}

pub struct HookedRead<R> {
    pub inner: R,
    pub knobs: Arc<Knobs>,
}

impl<R: ReadModelPort> ReadModelPort for HookedRead<R> {
    async fn conversation(
        &self,
        id: ConversationId,
        page: MessagePage,
    ) -> Result<ConversationView, StorageError> {
        self.inner.conversation(id, page).await
    }
    async fn conversations(
        &self,
        project: Option<ProjectId>,
    ) -> Result<Vec<ConversationSummary>, StorageError> {
        self.inner.conversations(project).await
    }
    async fn projects(&self) -> Result<Vec<ProjectSummary>, StorageError> {
        self.inner.projects().await
    }
    async fn assistants(&self) -> Result<Vec<iwork_domain::assistant::Assistant>, StorageError> {
        self.inner.assistants().await
    }
    async fn pending_items(&self) -> Result<Vec<PendingItem>, StorageError> {
        self.inner.pending_items().await
    }
    async fn quota(&self) -> Result<Vec<RuntimeQuota>, StorageError> {
        self.inner.quota().await
    }
    async fn runtimes(&self) -> Result<Vec<iwork_domain::runtime::Runtime>, StorageError> {
        self.inner.runtimes().await
    }
    async fn rules(
        &self,
        project: Option<ProjectId>,
    ) -> Result<Vec<iwork_domain::rule::RuleEntry>, StorageError> {
        self.inner.rules(project).await
    }
    async fn security_lists(&self) -> Result<Vec<iwork_domain::rule::RuleEntry>, StorageError> {
        self.inner.security_lists().await
    }
    async fn settings(&self) -> Result<iwork_domain::settings::GlobalSettings, StorageError> {
        self.inner.settings().await
    }
    async fn unsettled(&self) -> Result<Unsettled, StorageError> {
        self.knobs.unsettled.pass().await;
        self.inner.unsettled().await
    }
    async fn summary_source(
        &self,
        turn: TurnId,
        line: Option<ProjectId>,
        recent_limit: u32,
    ) -> Result<SummarySource, StorageError> {
        self.inner.summary_source(turn, line, recent_limit).await
    }
    async fn turn_work(&self) -> Result<TurnWork, StorageError> {
        self.knobs.turn_work.pass().await;
        self.inner.turn_work().await
    }
    async fn last_session_binding(
        &self,
        conversation: ConversationId,
    ) -> Result<Option<String>, StorageError> {
        self.knobs.last_session_binding.pass().await;
        self.inner.last_session_binding(conversation).await
    }
    async fn session_ids_of(
        &self,
        conversation: ConversationId,
    ) -> Result<Vec<String>, StorageError> {
        self.inner.session_ids_of(conversation).await
    }
    async fn quit_counts(&self) -> Result<QuitCounts, StorageError> {
        self.knobs.quit_counts.pass().await;
        self.inner.quit_counts().await
    }
    async fn turn_request(
        &self,
        turn: TurnId,
    ) -> Result<Option<iwork_domain::message::Message>, StorageError> {
        self.inner.turn_request(turn).await
    }
    async fn turn_with_conversation(
        &self,
        turn: TurnId,
    ) -> Result<
        Option<(
            iwork_domain::turn::Turn,
            iwork_domain::conversation::Conversation,
        )>,
        StorageError,
    > {
        self.knobs.turn_with_conversation.pass().await;
        self.inner.turn_with_conversation(turn).await
    }
    async fn snapshot_inputs(
        &self,
        conversation: ConversationId,
    ) -> Result<SnapshotInputs, StorageError> {
        self.inner.snapshot_inputs(conversation).await
    }
    async fn artifact_is_referenced(&self, r: &ArtifactRef) -> Result<bool, StorageError> {
        self.inner.artifact_is_referenced(r).await
    }
}
