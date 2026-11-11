//! 回合编排读历史消息的假读模型（T06 方案 5.2a）：只实现 `summary_source`，别的查询编排不用。
//! 没给原料时被读到就 panic：不该走续接失败那条路的用例一读就红。

use std::sync::Mutex;

use iwork_domain::assistant::Assistant;
use iwork_domain::common::ArtifactRef;
use iwork_domain::id::{ConversationId, ProjectId, TurnId};
use iwork_domain::rule::RuleEntry;
use iwork_domain::runtime::Runtime;
use iwork_domain::settings::GlobalSettings;
use iwork_ports::read_model::{
    ConversationSummary, ConversationView, MessagePage, PendingItem, ProjectSummary, ReadModelPort,
    RuntimeQuota, SummarySource, Unsettled,
};
use iwork_ports::store::StorageError;

#[derive(Default)]
pub struct FakeHistory {
    source: Mutex<Option<SummarySource>>,
    /// 每次读的参数：回合、会话线、条数上限
    pub calls: Mutex<Vec<(TurnId, Option<ProjectId>, u32)>>,
}

impl FakeHistory {
    /// 给出续接摘要的原料；之后被读到才不 panic。
    pub fn set(&self, source: SummarySource) {
        *self.source.lock().unwrap() = Some(source);
    }

    pub fn calls(&self) -> Vec<(TurnId, Option<ProjectId>, u32)> {
        self.calls.lock().unwrap().clone()
    }
}

const UNUSED: &str = "回合编排只读 summary_source";

impl ReadModelPort for FakeHistory {
    async fn summary_source(
        &self,
        turn: TurnId,
        line: Option<ProjectId>,
        recent_limit: u32,
    ) -> Result<SummarySource, StorageError> {
        self.calls.lock().unwrap().push((turn, line, recent_limit));
        Ok(self
            .source
            .lock()
            .unwrap()
            .clone()
            .expect("这条用例不该读历史消息"))
    }
    async fn conversation(
        &self,
        _id: ConversationId,
        _page: MessagePage,
    ) -> Result<ConversationView, StorageError> {
        unimplemented!("{UNUSED}")
    }
    async fn conversations(
        &self,
        _project: Option<ProjectId>,
    ) -> Result<Vec<ConversationSummary>, StorageError> {
        unimplemented!("{UNUSED}")
    }
    async fn projects(&self) -> Result<Vec<ProjectSummary>, StorageError> {
        unimplemented!("{UNUSED}")
    }
    async fn assistants(&self) -> Result<Vec<Assistant>, StorageError> {
        unimplemented!("{UNUSED}")
    }
    async fn pending_items(&self) -> Result<Vec<PendingItem>, StorageError> {
        unimplemented!("{UNUSED}")
    }
    async fn quota(&self) -> Result<Vec<RuntimeQuota>, StorageError> {
        unimplemented!("{UNUSED}")
    }
    async fn runtimes(&self) -> Result<Vec<Runtime>, StorageError> {
        unimplemented!("{UNUSED}")
    }
    async fn rules(&self, _project: Option<ProjectId>) -> Result<Vec<RuleEntry>, StorageError> {
        unimplemented!("{UNUSED}")
    }
    async fn security_lists(&self) -> Result<Vec<RuleEntry>, StorageError> {
        unimplemented!("{UNUSED}")
    }
    async fn settings(&self) -> Result<GlobalSettings, StorageError> {
        unimplemented!("{UNUSED}")
    }
    async fn unsettled(&self) -> Result<Unsettled, StorageError> {
        unimplemented!("{UNUSED}")
    }
    async fn artifact_is_referenced(&self, _r: &ArtifactRef) -> Result<bool, StorageError> {
        unimplemented!("{UNUSED}")
    }
}
