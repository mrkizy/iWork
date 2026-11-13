//! 助手配置的自动保存（步 7c）：网页改一项 → 进闸（store::assistant_edits）→ 按闸的决定发 `UpdateAssistant`；
//! 回包与摘要更新再推闸往前走。闸的规则与理由见 store::assistant_edits。

use iwork_proto::pb;

use super::{Session, offline_error};
use crate::session::dial::Dialer;
use crate::store::assistant_edits::{Field, Send};

impl<D: Dialer> Session<D> {
    /// 网页改了一项。断线时不接（D8），和别的命令一样回「离线」。
    pub(super) fn assistant_field(&mut self, id: &str, field: Field) -> Option<pb::IntentError> {
        if !self.live() {
            return Some(offline_error());
        }
        let send = self
            .store
            .step_assistant_edit(id, |e, base| e.edit(id, field, base));
        self.send_assistant_update(send);
        None
    }

    /// 在途的 `UpdateAssistant` 有了结果。CONFLICT 重拼重放；别的错误不重试，留给页面。
    pub(super) fn assistant_update_done(&mut self, id: &str, error: Option<&pb::IntentError>) {
        let send = match error {
            None => self
                .store
                .step_assistant_edit(id, |e, base| e.accepted(id, base)),
            Some(err) => {
                let conflict = err.code == "conflict";
                self.store
                    .step_assistant_edit(id, |e, base| e.rejected(id, conflict, &err.message, base))
            }
        };
        self.send_assistant_update(send);
    }

    /// 摘要可能更新过（每轮主循环）：还有没落定改动的助手各推一步。
    pub(super) fn drive_assistant_edits(&mut self) {
        let busy: Vec<String> = self
            .store
            .assistant_edits
            .busy()
            .map(str::to_owned)
            .collect();
        for id in busy {
            let send = self
                .store
                .step_assistant_edit(&id, |e, base| e.observed(&id, base));
            self.send_assistant_update(send);
        }
    }

    fn send_assistant_update(&mut self, send: Option<Send>) {
        if let Some(s) = send {
            self.send_command(
                pb::command::Kind::UpdateAssistant(pb::UpdateAssistant {
                    assistant_id: s.assistant_id,
                    config: Some(s.config),
                    expected_version: s.expected_version,
                }),
                None,
            );
        }
    }
}
