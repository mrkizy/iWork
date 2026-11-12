//! 首页「新建对话 → 设上首页选的模型与档位 → 发出首条消息 → 跳过去」（状态在 store::home）。
//!
//! 每一步等上一步受理再发：模型与档位要在首条消息之前设上，首个回合才用得上。
//! 只有配上了本次受理范围的那个新对话才会收到首条消息（store::created），别的跳转不会把草稿发出去。
//! 断线时在飞的命令留在 outbox、重连后按原 command_id 重发（幂等），这里不另管；
//! 重连来的是快照、认不出新对话时由 store 停下报原因。

use std::collections::VecDeque;

use iwork_proto::pb;
use pb::command::Kind as Cmd;

use super::{InFlight, Session};
use crate::session::dial::Dialer;
use crate::store::created::Purpose;
use crate::store::home::StartError;

/// 这条命令是「新建并发出」的哪一步：受理结果回到 [`Session::on_start_step`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartStep {
    Create,
    /// 设模型、设档位、发消息。
    Follow,
}

impl<D: Dialer> Session<D> {
    pub(super) fn start(&mut self, project_id: String, text: String) -> Option<pb::IntentError> {
        if self.store.home.start.is_some() {
            return Some(pb::IntentError {
                code: "starting".into(),
                message: "上一条还在发".into(),
                retryable: true,
            });
        }
        if !self.live() {
            return Some(super::offline_error());
        }
        self.store.start_begin(project_id.clone(), text);
        self.start_steps = None;
        self.send_start_step(
            Cmd::CreateConversation(pb::CreateConversation { project_id }),
            StartStep::Create,
        );
        None
    }

    /// 每轮调一次：新对话配上了、步骤还没排，就排出来发第一步。
    pub(super) fn advance_start(&mut self) {
        let Some(start) = &self.store.home.start else {
            self.start_steps = None;
            return;
        };
        let Some(conversation_id) = start.conversation_id.clone() else {
            return;
        };
        if self.start_steps.is_some() {
            return;
        }
        let mut steps = VecDeque::new();
        if let Some(model) = self.store.home.model.clone() {
            steps.push_back(Cmd::SetConversationModel(pb::SetConversationModel {
                conversation_id: conversation_id.clone(),
                model: Some(model),
            }));
        }
        if let Some(mode) = self.store.home.mode {
            steps.push_back(Cmd::SetApprovalMode(pb::SetApprovalMode {
                target: Some(pb::ApprovalModeTarget {
                    kind: Some(pb::approval_mode_target::Kind::ConversationId(
                        conversation_id.clone(),
                    )),
                }),
                mode: mode.into(),
                follow_supervisor: false,
            }));
        }
        steps.push_back(Cmd::SendMessage(pb::SendMessage {
            conversation_id,
            text: start.text.clone(),
            attachment_ids: Vec::new(),
        }));
        self.start_steps = Some(steps);
        self.next_start_step();
    }

    fn next_start_step(&mut self) {
        match self.start_steps.as_mut().and_then(VecDeque::pop_front) {
            Some(cmd) => self.send_start_step(cmd, StartStep::Follow),
            None => {
                self.start_steps = None;
                self.store.start_finished();
            }
        }
    }

    /// 某一步的受理结果（outbox 里带着 [`StartStep`] 的命令）。
    pub(super) fn on_start_step(
        &mut self,
        step: StartStep,
        accepted: Option<(u64, u64)>,
        error: Option<pb::IntentError>,
    ) {
        if self.store.home.start.is_none() {
            return;
        }
        match (step, error, accepted) {
            (StartStep::Create, Some(e), _) => {
                self.store.start_failed(StartError::Create(e.message))
            }
            (StartStep::Follow, Some(e), _) => {
                self.start_steps = None;
                self.store.start_failed(StartError::Send(e.message));
            }
            (StartStep::Create, None, Some((first, last))) => {
                self.store.create_accepted(first, last, Purpose::Start);
            }
            (StartStep::Follow, None, _) => self.next_start_step(),
            (StartStep::Create, None, None) => self
                .store
                .start_failed(StartError::Create("后台的回包里没有受理范围".into())),
        }
    }

    fn send_start_step(&mut self, kind: Cmd, step: StartStep) {
        let command_id = ulid::Ulid::generate().to_string();
        let command = pb::Command {
            command_id: command_id.clone(),
            kind: Some(kind),
        };
        self.outbox.insert(
            command_id,
            InFlight {
                command: command.clone(),
                reply: None,
                start_step: Some(step),
            },
        );
        self.dispatch(command);
    }
}
