//! 添加项目（T09 方案 15.2）：网页的意图作用在 Rust 的添加项目草稿上（store::add_project），会话层按草稿发
//! 检查（`InspectProjectPath`，附带查询）与命令（`InitRepository`、`AddProject`、`CreateProject`）。
//! 路径只从系统选择框、拖入进来（RD1）。克隆的客户端一半没接（intent.rs 的 CLONE_NOT_CONNECTED）。
//!
//! 检查不在每个操作里各发一次：每轮主循环看草稿里有没有「检查中、还没在飞」的候选，有就发（[`Session::want_inspect`]）。
//! 换候选、重查、断线后都走这一处（RD4：断线丢了的检查重连后重发）。

use iwork_ipc::client::ClientError;
use iwork_proto::pb;
use tokio::sync::oneshot;

use super::{Internal, Session, offline_error};
use crate::intent::AddProjectAct;
use crate::session::dial::Dialer;
use crate::store::add_project::Confirm;

/// 不能做时的错误（候选状态不对、正在忙）。
fn not_ready(message: &str) -> pb::IntentError {
    pb::IntentError {
        code: "not_ready".into(),
        message: message.into(),
        retryable: false,
    }
}

impl<D: Dialer> Session<D> {
    /// 添加项目一组的操作。要发命令的，受理结果（含后台的拒绝原因）回到 `tx`；不受理的原因同时记进草稿（弹窗里
    /// 显示，网页不再另弹提示）。
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
            AddProjectAct::InitCandidate | AddProjectAct::Confirm | AddProjectAct::Create { .. }
        );
        if needs_backend && !self.live() {
            return self.add_project_reject(tx, offline_error());
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
                Ok((op, path)) => self.send_draft_command(
                    op,
                    pb::command::Kind::InitRepository(pb::InitRepository { path }),
                    tx,
                ),
                Err(e) => self.add_project_reject(tx, not_ready(e)),
            },
            AddProjectAct::Confirm => match self.store.edit_add_project(|d| d.confirm()) {
                Ok(Confirm::Add {
                    path,
                    propose_draft,
                    op,
                }) => self.send_draft_command(
                    op,
                    pb::command::Kind::AddProject(pb::AddProject {
                        path,
                        propose_agents_md_draft: Some(propose_draft),
                    }),
                    tx,
                ),
                Ok(Confirm::Open { project_id }) => {
                    self.store.navigate_project(project_id);
                    reply(tx, None);
                }
                Err(e) => self.add_project_reject(tx, not_ready(e)),
            },
            AddProjectAct::Create { name } => match self
                .store
                .edit_add_project(|d| d.begin_create())
            {
                Ok(op) => {
                    let parent_dir = self.store.add_project.parent_dir.clone();
                    self.send_draft_command(
                        op,
                        pb::command::Kind::CreateProject(pb::CreateProject { name, parent_dir }),
                        tx,
                    );
                }
                Err(e) => self.add_project_reject(tx, not_ready(e)),
            },
        }
    }

    /// 不受理：原因记进草稿（弹窗里显示），也回给这次操作。
    fn add_project_reject(&mut self, tx: oneshot::Sender<pb::IntentAck>, e: pb::IntentError) {
        let why = e.message.clone();
        self.store.edit_add_project(|d| d.error = Some(why));
        let _ = tx.send(pb::IntentAck { error: Some(e) });
    }

    /// 发草稿操作 `op` 的命令，记下 command_id → 编号，回包按编号交回草稿。
    fn send_draft_command(
        &mut self,
        op: u64,
        kind: pb::command::Kind,
        tx: oneshot::Sender<pb::IntentAck>,
    ) {
        if let Some(command_id) = self.send_command(kind, Some(tx)) {
            self.add_project_ops.insert(command_id, op);
        }
    }

    /// 草稿操作 `op` 的命令有了结果。`accepted` 是受理的 seq 范围。草稿只认当前那件（弹窗关过的旧操作不动新草稿）。
    pub(super) fn add_project_done(
        &mut self,
        op: u64,
        kind: &Option<pb::command::Kind>,
        accepted: Option<(u64, u64)>,
        error: Option<&pb::IntentError>,
    ) {
        use pb::command::Kind as C;
        if let Some(e) = error {
            let why = e.message.clone();
            self.store.edit_add_project(|d| d.finished(op, Err(why)));
            return;
        }
        match (kind, accepted) {
            (Some(C::AddProject(_) | C::CreateProject(_)), Some((first, last))) => {
                self.store.project_add_accepted(first, last, op);
            }
            _ => {
                self.store.edit_add_project(|d| d.finished(op, Ok(())));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::session;
    use crate::store::add_project::{Busy, Check};

    fn unversioned(root: &str) -> pb::InspectProjectPathResult {
        pb::InspectProjectPathResult {
            root_path: root.into(),
            state: pb::ProjectPathState::NotVersionControlled.into(),
            ..Default::default()
        }
    }

    /// 审查 g3 1/F-2（复现原样）：初始化 A 在途时关掉弹窗、再初始化 B；A 的受理回包不能结束 B 的初始化。
    #[tokio::test]
    async fn old_init_reply_does_not_finish_new_draft() {
        let mut s = session();
        let d = &mut s.store.add_project;
        let t = d.set_candidate("/A".into()).token;
        d.inspected(t, Ok(unversioned("/A")));
        let (a, _) = d.init().unwrap();
        d.clear();
        let t = d.set_candidate("/B".into()).token;
        d.inspected(t, Ok(unversioned("/B")));
        let (b, _) = d.init().unwrap();
        let init = Some(pb::command::Kind::InitRepository(pb::InitRepository {
            path: "/A".into(),
        }));
        s.add_project_done(a, &init, Some((10, 10)), None);
        assert_eq!(s.store.add_project.busy_kind(), Some(Busy::Initializing));
        s.add_project_done(b, &init, Some((11, 11)), None);
        assert_eq!(s.store.add_project.busy, None, "对照：B 自己的回包结束它");
    }

    /// 审查 g3 附注：重连后候选 token 不变；旧连接上迟到的成功回包不记，只认本连接上发的那次。
    #[tokio::test]
    async fn old_connection_success_for_unchanged_candidate_is_dropped() {
        let mut s = session();
        s.conn_epoch = 1;
        let token = s.store.add_project.set_candidate("/same".into()).token;
        s.conn_epoch = 2;
        s.inspect_in_flight = Some((2, token));
        let ok = pb::InspectProjectPathResult {
            root_path: "/same".into(),
            state: pb::ProjectPathState::Addable.into(),
            ..Default::default()
        };
        s.on_inspected(1, token, Ok(ok.clone()));
        assert_eq!(
            s.store.add_project.candidate.as_ref().unwrap().check,
            Check::Checking
        );
        assert_eq!(s.inspect_in_flight, Some((2, token)), "本连接那次还在飞");
        s.on_inspected(2, token, Ok(ok));
        assert!(matches!(
            s.store.add_project.candidate.as_ref().unwrap().check,
            Check::Done(_)
        ));
    }
}
