//! 对话删除期间的命令放行（方案 4.3 对话表）。

mod support;

use iwork_app::admission::{AdmitError, DELETING_ALLOWED, admit, cleanup_kind, command_name};
use iwork_domain::DomainError;
use iwork_domain::action::CanonicalPath;
use iwork_domain::common::{Sha256, Timestamp};
use iwork_domain::conversation::{Conversation, ConversationKind, ConversationState};
use iwork_domain::id::{ConversationId, ProjectId, RawUlid, Seq, TurnId, UserActionId};
use iwork_domain::user_action::{Proposal, UserActionCard, UserActionState};
use iwork_proto::pb;
use prost::Message;
use support::oneof_fields;

fn conversation(state: ConversationState) -> Conversation {
    Conversation {
        id: ConversationId::from_raw(RawUlid::from_u128(1)),
        kind: ConversationKind::Project {
            project_id: ProjectId::from_raw(RawUlid::from_u128(2)),
        },
        state,
        last_read_seq: Seq(0),
        model_override: None,
        approval_mode: None,
        updated_at: Timestamp(0),
    }
}

fn card(conversation_id: ConversationId, state: UserActionState) -> UserActionCard {
    UserActionCard {
        id: UserActionId::from_raw(RawUlid::from_u128(3)),
        turn_id: TurnId::from_raw(RawUlid::from_u128(4)),
        conversation_id,
        proposal: Proposal::Command {
            command: "echo".into(),
            cwd: CanonicalPath("/".into()),
        },
        reason: String::new(),
        proposal_hash: Sha256([0; 32]),
        state,
        output_artifact: None,
    }
}

/// 从描述符集穷举 proto `Command` 的全部 oneof 变体：对每个字段编号编出「该字段 + 空消息」
/// 的字节再解码，得到那个变体。proto 加命令不用改这里。
fn every_command() -> Vec<(String, pb::command::Kind)> {
    oneof_fields("Command")
        .into_iter()
        .map(|(number, name)| {
            let key = (number as u64) << 3 | 2; // 长度前缀类型
            let mut bytes = Vec::new();
            prost::encoding::encode_varint(key, &mut bytes);
            bytes.push(0); // 空子消息
            let command =
                pb::Command::decode(bytes.as_slice()).unwrap_or_else(|e| panic!("{name}：{e}"));
            (
                name,
                command
                    .kind
                    .unwrap_or_else(|| panic!("{} 没解出变体", number)),
            )
        })
        .collect()
}

#[test]
fn command_names_match_proto_oneof() {
    let commands = every_command();
    assert_eq!(commands.len(), 44, "对照：proto 命令数");
    for (name, kind) in &commands {
        assert_eq!(command_name(kind), name);
    }
    for (name, _) in DELETING_ALLOWED {
        assert!(
            commands.iter().any(|(n, _)| n == name),
            "登记表里的 {name} 不是 proto 命令"
        );
    }
}

/// `Deleting` 下：登记表里的三条收尾命令（满足领域条件时）受理，其余全部 `ConversationDeleting`。
#[test]
fn conversation_deleting_allows_only_listed_commands() {
    let deleting = conversation(ConversationState::Deleting);
    let unknown = card(
        deleting.id,
        UserActionState::Unknown {
            error: "后台服务中断".into(),
        },
    );
    let executing = card(deleting.id, UserActionState::Executing);

    let mut accepted = Vec::new();
    for (name, kind) in every_command() {
        // 给每条命令配上让领域条件成立的事实；不在登记表里的命令事实无关
        let card = match name.as_str() {
            "stop_user_action" => Some(&executing),
            _ => Some(&unknown),
        };
        match admit(&deleting, &kind, card, true) {
            Ok(()) => accepted.push(name),
            Err(e) => {
                assert!(
                    cleanup_kind(&kind).is_none(),
                    "{name} 在登记表里却被拒：{e}"
                );
                assert_eq!(
                    e,
                    AdmitError::Domain(DomainError::ConversationDeleting { id: deleting.id }),
                    "{name}"
                );
            }
        }
    }
    let mut listed: Vec<String> = DELETING_ALLOWED
        .iter()
        .map(|(n, _)| (*n).to_owned())
        .collect();
    listed.sort();
    accepted.sort();
    assert_eq!(accepted, listed);

    // 领域条件仍然起作用：别的对话的卡片、没有句柄的停止都拒
    let stop = every_command()
        .into_iter()
        .find(|(n, _)| n == "stop_user_action")
        .unwrap()
        .1;
    assert!(matches!(
        admit(&deleting, &stop, Some(&executing), false),
        Err(AdmitError::Domain(
            DomainError::UserActionNotStoppable { .. }
        ))
    ));
    let confirm = every_command()
        .into_iter()
        .find(|(n, _)| n == "confirm_user_action_ended")
        .unwrap()
        .1;
    let foreign = card(
        ConversationId::from_raw(RawUlid::from_u128(9)),
        UserActionState::Unknown { error: "x".into() },
    );
    assert!(matches!(
        admit(&deleting, &confirm, Some(&foreign), true),
        Err(AdmitError::Domain(
            DomainError::CardNotInConversation { .. }
        ))
    ));
    assert_eq!(
        admit(&deleting, &confirm, None, true),
        Err(AdmitError::CardNotLoaded {
            command: "confirm_user_action_ended"
        })
    );

    // 对照：Active 下普通命令照常受理
    let send = every_command()
        .into_iter()
        .find(|(n, _)| n == "send_message")
        .unwrap()
        .1;
    assert_eq!(
        admit(&conversation(ConversationState::Active), &send, None, false),
        Ok(())
    );
}
