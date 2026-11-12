//! 添加项目（T09 方案 15.2）：网页的意图作用在 Rust 的添加项目草稿上（store::add_project），会话层按草稿发
//! 检查（`InspectProjectPath`，附带查询）与命令（`InitRepository`、`AddProject`、`CreateProject`、`CloneRepository`）。
//! 路径只从系统选择框、拖入、`CloneFinished` 进来（RD1）。
//!
//! 检查不在每个操作里各发一次：每轮主循环看草稿里有没有「检查中、还没在飞」的候选，有就发（[`Session::want_inspect`]）。
//! 换候选、重查、断线后都走这一处（RD4：断线丢了的检查重连后重发）。

use iwork_ipc::client::ClientError;
use iwork_proto::pb;
use tokio::sync::oneshot;

use super::{Internal, Session, offline_error};
use crate::intent::AddProjectAct;
use crate::session::dial::Dialer;
use crate::store::add_project::{Busy, Confirm};

/// 不能做时的错误（候选状态不对、正在忙）。
fn not_ready(message: &str) -> pb::IntentError {
    pb::IntentError {
        code: "not_ready".into(),
        message: message.into(),
        retryable: false,
    }
}

impl<D: Dialer> Session<D> {
    /// 添加项目一组的操作。要发命令的，受理结果（含后台的拒绝原因）回到 `tx`，网页照常弹提示。
    pub(super) fn add_project_act(
        &mut self,
        act: AddProjectAct,
        tx: oneshot::Sender<pb::IntentAck>,
    ) {
        let reply = |tx: oneshot::Sender<pb::IntentAck>, error: Option<pb::IntentError>| {
            let _ = tx.send(pb::IntentAck { error });
        };
        let needs_backend = matches!(
            act,
            AddProjectAct::InitCandidate
                | AddProjectAct::Confirm
                | AddProjectAct::Create { .. }
                | AddProjectAct::StartClone { .. }
                | AddProjectAct::RetryClone
        );
        if needs_backend && !self.live() {
            return reply(tx, Some(offline_error()));
        }
        match act {
            AddProjectAct::SetProposeDraft(on) => {
                self.store.edit_add_project(|d| d.propose_draft = on);
                reply(tx, None);
            }
            AddProjectAct::Clear => {
                self.store.edit_add_project(|d| d.clear());
                reply(tx, None);
            }
            AddProjectAct::InitCandidate => match self.store.edit_add_project(|d| d.init()) {
                Ok(path) => self.send_command(
                    pb::command::Kind::InitRepository(pb::InitRepository { path }),
                    Some(tx),
                ),
                Err(e) => reply(tx, Some(not_ready(e))),
            },
            AddProjectAct::Confirm => match self.store.edit_add_project(|d| d.confirm()) {
                Ok(Confirm::Add {
                    path,
                    propose_draft,
                }) => self.send_command(
                    pb::command::Kind::AddProject(pb::AddProject {
                        path,
                        propose_agents_md_draft: Some(propose_draft),
                    }),
                    Some(tx),
                ),
                Ok(Confirm::Open { project_id }) => {
                    self.store.navigate_project(project_id);
                    reply(tx, None);
                }
                Err(e) => reply(tx, Some(not_ready(e))),
            },
            AddProjectAct::Create { name } => match self
                .store
                .edit_add_project(|d| d.begin(Busy::Creating))
            {
                Ok(()) => {
                    let parent_dir = self.store.add_project.parent_dir.clone();
                    self.send_command(
                        pb::command::Kind::CreateProject(pb::CreateProject { name, parent_dir }),
                        Some(tx),
                    );
                }
                Err(e) => reply(tx, Some(not_ready(e))),
            },
            AddProjectAct::StartClone { url, name } => {
                let parent_dir = self.store.add_project.parent_dir.clone();
                self.start_clone(
                    pb::CloneRepository {
                        url,
                        parent_dir,
                        name,
                    },
                    tx,
                );
            }
            AddProjectAct::RetryClone => match self.last_clone.clone() {
                Some(c) => self.start_clone(c, tx),
                None => reply(tx, Some(not_ready("还没有克隆过"))),
            },
        }
    }

    fn start_clone(&mut self, c: pb::CloneRepository, tx: oneshot::Sender<pb::IntentAck>) {
        if let Err(e) = self.store.edit_add_project(|d| d.begin(Busy::Cloning)) {
            let _ = tx.send(pb::IntentAck {
                error: Some(not_ready(e)),
            });
            return;
        }
        // 参数记在会话层，「重试」不让网页重传
        self.last_clone = Some(c.clone());
        self.send_command(pb::command::Kind::CloneRepository(c), Some(tx));
    }

    /// 添加项目一组的命令有了结果。`accepted` 是受理的 seq 范围。
    pub(super) fn add_project_done(
        &mut self,
        kind: &pb::command::Kind,
        accepted: Option<(u64, u64)>,
        error: Option<&pb::IntentError>,
    ) {
        use pb::command::Kind as C;
        if !matches!(
            kind,
            C::InitRepository(_) | C::AddProject(_) | C::CreateProject(_) | C::CloneRepository(_)
        ) {
            return;
        }
        if let Some(e) = error {
            let why = e.message.clone();
            self.store.edit_add_project(|d| d.finished(Err(why)));
            return;
        }
        match (kind, accepted) {
            (C::AddProject(_) | C::CreateProject(_), Some((first, last))) => {
                self.store.project_add_accepted(first, last);
            }
            // 克隆受理了：等 CloneFinished（store 里收）
            (C::CloneRepository(_), _) => {}
            _ => {
                self.store.edit_add_project(|d| d.finished(Ok(())));
            }
        }
    }

    /// 选择框选好了一个文件夹：设成候选（检查由 want_inspect 发），或记成这一次的父目录。
    pub(super) fn add_project_picked(&mut self, pick: AddProjectPick, path: String) {
        match pick {
            AddProjectPick::Candidate => self.store.edit_add_project(|d| {
                d.set_candidate(path);
            }),
            AddProjectPick::Parent => self.store.edit_add_project(|d| d.parent_dir = Some(path)),
        }
    }

    /// 拖进来的恰好是一个目录：设成候选、请网页打开添加项目弹窗。返回是否接手了（否则照旧当附件）。
    /// 只读元数据看是不是目录，不读内容、不认仓库（RD1 + 第 1 节约束）。
    pub(super) fn dropped_folder(&mut self, paths: &[String]) -> bool {
        let [only] = paths else {
            return false;
        };
        if !std::fs::metadata(only).is_ok_and(|m| m.is_dir()) {
            return false;
        }
        self.store.edit_add_project(|d| {
            d.set_candidate(only.clone());
        });
        self.store.navigate_add_project();
        true
    }

    /// 每轮主循环：草稿里有检查中、还没在这条连接上发出去的候选，就发检查。
    pub(super) fn want_inspect(&mut self) {
        if !self.live() {
            return;
        }
        let Some(want) = self.store.add_project.pending_inspect() else {
            return;
        };
        if self.inspect_in_flight == Some((self.conn_epoch, want.token)) {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        self.inspect_in_flight = Some((self.conn_epoch, want.token));
        let tx = self.internal_tx.clone();
        let epoch = self.conn_epoch;
        tokio::spawn(async move {
            let query = pb::Query {
                kind: Some(pb::query::Kind::InspectProjectPath(
                    pb::InspectProjectPath { path: want.path },
                )),
            };
            let result = match client.request(pb::request::Body::Query(query)).await {
                Ok(r) => match r.body {
                    Some(pb::response::Body::Result(pb::QueryResult {
                        kind: Some(pb::query_result::Kind::InspectProjectPath(x)),
                        ..
                    })) => Ok(x),
                    Some(pb::response::Body::Error(e)) => Err(Some(e.message)),
                    other => Err(Some(format!("InspectProjectPath 的回包不对：{other:?}"))),
                },
                // 连接断了：不算检查失败，重连后照常重发（RD4）
                Err(ClientError::Closed) => Err(None),
                Err(e) => Err(Some(e.to_string())),
            };
            let _ = tx.send(Internal::Inspected {
                epoch,
                token: want.token,
                result,
            });
        });
    }

    pub(super) fn on_inspected(
        &mut self,
        epoch: u64,
        token: u64,
        result: Result<pb::InspectProjectPathResult, Option<String>>,
    ) {
        if self.inspect_in_flight == Some((epoch, token)) {
            self.inspect_in_flight = None;
        }
        if epoch != self.conn_epoch {
            return;
        }
        let result = match result {
            Ok(r) => Ok(r),
            Err(Some(why)) => Err(why),
            // 连接断了：候选仍是「检查中」，重连后 want_inspect 重发
            Err(None) => return,
        };
        self.store.edit_add_project(|d| d.inspected(token, result));
    }
}

/// 选择框选的文件夹作什么用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AddProjectPick {
    Candidate,
    Parent,
}
