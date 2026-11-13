//! 查询受理（T10a 方案 2.6 查询表；0a.3 的 Q1、Q2）。
//!
//! - 视图查询在**一个**读事务里取完，`QueryResult.seq` 填这个事务看到的 head（Q1）；
//! - 只读、不提交、不改内存，被中止、重发、乱序到达都没事（Q2）；
//! - 订阅快照（[`crate::snapshot`]）调的是同一个 [`answer_in`]，不另写一套（S1）。
//!
//! 每种查询在哪儿答由 [`place`] 一处定：视图查询开读事务；读内存的（点通知记下的目标）不碰库；还没接的（制品、
//! 模型、本机配置、安全清单、检查项目路径）回 `UNAVAILABLE`「未接入」，由各自的卡来换。

use std::sync::Arc;

use iwork_app::convert::{
    ConvertError, assistant_to_proto, project_settings_to_proto, rule_to_proto, runtime_to_proto,
};
use iwork_app::views;
use iwork_domain::id::Seq;
use iwork_ipc::server::{ChunkRefused, ConnectionContext, RequestHandler};
use iwork_ports::store::{EventCodec, StorageError};
use iwork_proto::pb;
use iwork_proto::pb::query::Kind as Q;
use iwork_proto::pb::query_result::Kind as R;
use iwork_store::{SqliteReadModel, ViewTx};

use crate::attention::Attention;

/// 存储故障 → 对外错误：忙可重试，别的是内部错误（日志里点名）。
pub fn storage_error(e: &StorageError) -> pb::Error {
    tracing::warn!(error = %e, "查询读库失败");
    pb::Error {
        code: if e.retryable() {
            pb::ErrorCode::Unavailable
        } else {
            pb::ErrorCode::Internal
        }
        .into(),
        message: e.to_string(),
        retryable: e.retryable(),
        details: Default::default(),
    }
}

/// 转换失败是程序缺陷（读出来的实体转不成协议）：内部错误，日志点名。
fn convert_error(e: &ConvertError) -> pb::Error {
    tracing::error!(error = %e, "查询结果转换失败");
    pb::Error {
        code: pb::ErrorCode::Internal.into(),
        message: e.to_string(),
        retryable: false,
        details: Default::default(),
    }
}

fn unwired(what: &str) -> pb::Error {
    pb::Error {
        code: pb::ErrorCode::Unavailable.into(),
        message: format!("未接入：{what}"),
        retryable: false,
        details: Default::default(),
    }
}

/// 一条查询的失败：请求不成立（直接回给客户端），或者读库出错。
enum Failure {
    Refused(pb::Error),
    Storage(StorageError),
}

impl From<StorageError> for Failure {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

impl From<pb::Error> for Failure {
    fn from(e: pb::Error) -> Self {
        Self::Refused(e)
    }
}

impl From<ConvertError> for Failure {
    fn from(e: ConvertError) -> Self {
        Self::Refused(convert_error(&e))
    }
}

/// 一条查询在哪儿答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// 视图查询：一个读事务里答（[`answer_in`]）
    View,
    /// 读后台进程的内存，不碰库（Q2 的例外）
    Memory,
    /// 还没接：回「未接入」，括号里是归哪张卡
    Unwired(&'static str),
}

/// 查询种类 → 在哪儿答。穷举：加一种查询要在这里表态。
pub fn place(kind: &Q) -> Place {
    match kind {
        Q::ReadArtifact(_) => Place::Unwired("读制品（T10d）"),
        Q::ListModels(_) => Place::Unwired("模型列表（T10g）"),
        Q::ListLocalConfig(_) => Place::Unwired("本机配置（T10f）"),
        Q::ListSecurityLists(_) => Place::Unwired("安全清单（T10e）"),
        Q::InspectProjectPath(_) => Place::Unwired("检查项目路径（T10c）"),
        Q::GetOpenTarget(_) => Place::Memory,
        Q::GetConversation(_)
        | Q::ListConversations(_)
        | Q::ListProjects(_)
        | Q::ListAssistants(_)
        | Q::ListPendingItems(_)
        | Q::GetQuota(_)
        | Q::GetRuntimeStatus(_)
        | Q::ListRules(_)
        | Q::GetSettings(_)
        | Q::ListAnchors(_) => Place::View,
    }
}

/// 一条视图查询在读事务 `tx` 里的答案，`seq` 填 `head`。读库出错时整个回 `Err`（订阅快照据此放弃这次快照）；
/// 请求不成立时回 `Ok(Err(..))`。
pub fn answer_in<C: EventCodec + ?Sized>(
    tx: &ViewTx<'_, C>,
    head: Seq,
    kind: &Q,
) -> Result<Result<pb::QueryResult, pb::Error>, StorageError> {
    match view(tx, kind) {
        Ok(result) => Ok(Ok(pb::QueryResult {
            seq: head.0,
            kind: Some(result),
        })),
        Err(Failure::Refused(e)) => Ok(Err(e)),
        Err(Failure::Storage(e)) => Err(e),
    }
}

fn view<C: EventCodec + ?Sized>(tx: &ViewTx<'_, C>, kind: &Q) -> Result<R, Failure> {
    let out = match kind {
        Q::GetConversation(g) => {
            let (id, paging) = views::conversation_paging(g)?;
            let page = tx
                .conversation_page(id, paging)?
                .map_err(|r| views::refused_to_error(&r))?;
            R::GetConversation(views::conversation_page_to_proto(&page)?)
        }
        Q::ListConversations(l) => {
            let all = tx.conversations(views::project_filter(l)?)?;
            R::ListConversations(views::list_conversations(&all, l)?)
        }
        Q::ListProjects(_) => R::ListProjects(pb::ListProjectsResult {
            projects: tx
                .projects()?
                .iter()
                .map(views::project_summary_to_proto)
                .collect::<Result<_, _>>()?,
        }),
        Q::ListAssistants(_) => R::ListAssistants(pb::ListAssistantsResult {
            assistants: tx.assistants()?.iter().map(assistant_to_proto).collect(),
            // 助手的最近动态（G7）归 T10b
            activities: Vec::new(),
        }),
        Q::ListPendingItems(_) => R::ListPendingItems(pb::ListPendingItemsResult {
            items: tx
                .pending()?
                .iter()
                .map(views::pending_entry_to_proto)
                .collect::<Result<_, _>>()?,
        }),
        Q::GetQuota(_) => R::GetQuota(pb::GetQuotaResult {
            runtimes: tx
                .quota()?
                .iter()
                .map(views::runtime_quota_to_proto)
                .collect::<Result<_, _>>()?,
        }),
        Q::GetRuntimeStatus(_) => R::GetRuntimeStatus(pb::GetRuntimeStatusResult {
            runtimes: tx
                .runtimes()?
                .iter()
                .map(runtime_to_proto)
                .collect::<Result<_, _>>()?,
        }),
        Q::ListRules(l) => {
            let project = l
                .project_id
                .as_deref()
                .map(|p| {
                    p.parse()
                        .map_err(|_| views::invalid("project_id", format!("不是合法的编号：{p}")))
                })
                .transpose()?;
            R::ListRules(pb::ListRulesResult {
                // 安全清单的条目也是规则条目，归 ListSecurityLists（T10e）
                rules: tx
                    .rules(project)?
                    .iter()
                    .filter(|r| r.security_list().is_none())
                    .map(rule_to_proto)
                    .collect::<Result<_, _>>()?,
            })
        }
        Q::GetSettings(g) => {
            let project = match g.project_id.as_deref() {
                None => None,
                Some(p) => {
                    let id = p.parse().map_err(|_| {
                        views::invalid("project_id", format!("不是合法的编号：{p}"))
                    })?;
                    let found = tx.project(id)?.ok_or_else(|| pb::Error {
                        code: pb::ErrorCode::NotFound.into(),
                        message: format!("项目不存在：{p}"),
                        retryable: false,
                        details: Default::default(),
                    })?;
                    Some(project_settings_to_proto(&found.settings))
                }
            };
            R::GetSettings(pb::GetSettingsResult {
                global: Some(views::global_settings_to_proto(&tx.settings()?)),
                project,
            })
        }
        Q::ListAnchors(a) => {
            let (id, window) = views::anchor_window(a)?;
            let anchors = tx
                .anchors(id, window)?
                .map_err(|r| views::refused_to_error(&r))?;
            R::ListAnchors(views::anchors_to_proto(&anchors))
        }
        Q::ReadArtifact(_)
        | Q::ListModels(_)
        | Q::ListLocalConfig(_)
        | Q::ListSecurityLists(_)
        | Q::GetOpenTarget(_)
        | Q::InspectProjectPath(_) => unreachable!("不是视图查询，见 place()"),
    };
    Ok(out)
}

/// 查询的受理方。
pub struct Queries<C: EventCodec> {
    read: Arc<SqliteReadModel<C>>,
    /// 点通知记下的目标（`GetOpenTarget` 读它，不开读事务）
    attention: Arc<Attention>,
}

impl<C: EventCodec> Clone for Queries<C> {
    fn clone(&self) -> Self {
        Self {
            read: self.read.clone(),
            attention: self.attention.clone(),
        }
    }
}

impl<C: EventCodec + 'static> Queries<C> {
    pub fn new(read: Arc<SqliteReadModel<C>>, attention: Arc<Attention>) -> Self {
        Self { read, attention }
    }

    pub fn read_model(&self) -> &Arc<SqliteReadModel<C>> {
        &self.read
    }

    /// 读内存的查询（[`Place::Memory`]）。
    fn answer_in_memory(&self, kind: &Q) -> R {
        match kind {
            Q::GetOpenTarget(_) => R::GetOpenTarget(iwork_app::notify::open_target_to_proto(
                self.attention.open_target().as_ref(),
            )),
            other => unreachable!("不是读内存的查询：{other:?}"),
        }
    }

    /// 答一条查询：视图查询开一个读事务。
    pub async fn answer(&self, query: pb::Query) -> Result<pb::QueryResult, pb::Error> {
        let Some(kind) = query.kind else {
            return Err(views::invalid("query", "查询为空"));
        };
        match place(&kind) {
            Place::View => {}
            Place::Unwired(what) => return Err(unwired(what)),
            // `seq` 填 0：读的是内存，不对应库的任何一刻
            Place::Memory => {
                return Ok(pb::QueryResult {
                    seq: 0,
                    kind: Some(self.answer_in_memory(&kind)),
                });
            }
        }
        let read = self.read.view(move |tx| {
            let head = tx.head()?;
            answer_in(tx, head, &kind)
        });
        match read.await {
            Ok(answer) => answer,
            Err(e) => Err(storage_error(&e)),
        }
    }
}

/// 请求入口的内层：查询自己答，别的（命令、分片）交给 `commands`（组 B 的分发入口；没接时是 `NotWired`）。
pub struct QueryOr<C: EventCodec, I> {
    queries: Queries<C>,
    commands: I,
}

impl<C: EventCodec, I> QueryOr<C, I> {
    pub fn new(queries: Queries<C>, commands: I) -> Self {
        Self { queries, commands }
    }
}

impl<C: EventCodec + 'static, I: RequestHandler> RequestHandler for QueryOr<C, I> {
    async fn handle(&self, ctx: &ConnectionContext, req: pb::Request) -> pb::Response {
        let request_id = req.request_id;
        match req.body {
            Some(pb::request::Body::Query(q)) => match self.queries.answer(q).await {
                Ok(result) => pb::Response {
                    request_id,
                    body: Some(pb::response::Body::Result(result)),
                },
                Err(e) => pb::Response {
                    request_id,
                    body: Some(pb::response::Body::Error(e)),
                },
            },
            body => {
                self.commands
                    .handle(ctx, pb::Request { request_id, body })
                    .await
            }
        }
    }

    async fn chunk(&self, ctx: &ConnectionContext, chunk: pb::Chunk) -> Result<(), ChunkRefused> {
        self.commands.chunk(ctx, chunk).await
    }

    async fn connection_closed(&self, ctx: &ConnectionContext) {
        // 连接级的状态随连接清掉（S5）：这条连接报的在看哪
        self.queries.attention.connection_closed(ctx.connection_id);
        self.commands.connection_closed(ctx).await;
    }
}
