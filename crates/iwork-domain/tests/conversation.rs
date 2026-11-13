//! 对话状态机与删除期间的命令放行（方案 4.3「对话 ConversationState」）。

mod support;

use iwork_domain::DomainError;
use iwork_domain::action::CanonicalPath;
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::common::{Sha256, Timestamp};
use iwork_domain::conversation::{
    self, CONVERSATION_MACHINE, CONVERSATION_TRANSITIONS, CleanupCommand, Conversation,
    ConversationKind, ConversationState,
};
use iwork_domain::id::{
    AssistantId, ConversationId, ProjectId, RawUlid, Seq, TurnId, UserActionId,
};
use iwork_domain::user_action::{Proposal, UserActionCard, UserActionState, UserActionStateKind};
use support::verify_machine;

use ConversationState as S;

fn conv_id(n: u128) -> ConversationId {
    ConversationId::from_raw(RawUlid::from_u128(n))
}

fn project_conversation(state: ConversationState) -> Conversation {
    Conversation {
        id: conv_id(1),
        kind: ConversationKind::Project {
            project_id: ProjectId::from_raw(RawUlid::from_u128(9)),
        },
        state,
        last_read_seq: Seq(0),
        model_override: None,
        approval_mode: None,
        updated_at: Timestamp(0),
    }
}

fn card(conversation: ConversationId, state: UserActionState) -> UserActionCard {
    UserActionCard {
        id: UserActionId::from_raw(RawUlid::from_u128(5)),
        turn_id: TurnId::from_raw(RawUlid::from_u128(6)),
        conversation_id: conversation,
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

#[test]
fn conversation_table_verified() {
    assert_eq!(CONVERSATION_TRANSITIONS.len(), 2, "对照：表行数");
    assert_eq!(
        verify_machine(
            CONVERSATION_MACHINE,
            conversation::next,
            CONVERSATION_TRANSITIONS
        ),
        Ok(())
    );
}

#[test]
fn conversation_deleting_allows_only_listed_commands() {
    let c = project_conversation(S::Deleting);
    let unknown = card(
        c.id,
        UserActionState::Unknown {
            error: "后台服务中断".into(),
        },
    );
    let executing = card(c.id, UserActionState::Executing);

    // 普通命令（SendMessage、DecideApproval……）一律 CONVERSATION_DELETING
    assert_eq!(
        c.admit_command(),
        Err(DomainError::ConversationDeleting { id: c.id })
    );

    // 删除停在 Deleting 就是在等用户确认结果未知的卡片
    assert_eq!(
        c.admit_cleanup(CleanupCommand::ConfirmUserActionEnded { card: &unknown }),
        Ok(())
    );
    // 停止：执行中、且本次后台服务仍持有句柄
    assert_eq!(
        c.admit_cleanup(CleanupCommand::StopUserAction {
            card: &executing,
            handle_held: true
        }),
        Ok(())
    );
    assert_eq!(
        c.admit_cleanup(CleanupCommand::StopUserAction {
            card: &executing,
            handle_held: false
        }),
        Err(DomainError::UserActionNotStoppable {
            card: executing.id,
            state: UserActionStateKind::Executing,
            handle_held: false,
        })
    );
    assert!(
        c.admit_cleanup(CleanupCommand::StopUserAction {
            card: &unknown,
            handle_held: true
        })
        .is_err()
    );
    // 同一对话重发删除：幂等
    assert_eq!(c.admit_cleanup(CleanupCommand::DeleteConversation), Ok(()));

    // 别的对话的卡片不能借道
    let foreign = card(conv_id(2), UserActionState::Unknown { error: "x".into() });
    assert_eq!(
        c.admit_cleanup(CleanupCommand::ConfirmUserActionEnded { card: &foreign }),
        Err(DomainError::CardNotInConversation {
            card: foreign.id,
            conversation: c.id
        })
    );

    // 对照：Active 下普通命令照常受理
    assert_eq!(project_conversation(S::Active).admit_command(), Ok(()));

    // 已删除：收尾命令也拒
    let deleted = project_conversation(S::Deleted);
    assert_eq!(
        deleted.admit_cleanup(CleanupCommand::ConfirmUserActionEnded { card: &unknown }),
        Err(DomainError::ConversationDeleted { id: deleted.id })
    );
}

#[test]
fn conversation_delete_flow() {
    let mut c = project_conversation(S::Active);
    c.request_delete().unwrap();
    assert_eq!(c.state, S::Deleting);
    // 重复发起不是转换：调用方按幂等处理
    assert!(matches!(
        c.request_delete(),
        Err(DomainError::IllegalTransition { .. })
    ));
    c.settle().unwrap();
    assert_eq!(c.state, S::Deleted);
}

#[test]
fn assistant_dm_cannot_be_deleted() {
    let mut c = project_conversation(S::Active);
    c.kind = ConversationKind::AssistantDm {
        assistant_id: AssistantId::from_raw(RawUlid::from_u128(7)),
        context_project: None,
    };
    assert_eq!(
        c.request_delete(),
        Err(DomainError::AssistantDmNotDeletable { id: c.id })
    );
    assert_eq!(c.state, S::Active);
}

#[test]
fn dm_project_switch_clears_own_mode() {
    let p = ProjectId::from_raw(RawUlid::from_u128(8));
    let mut c = project_conversation(S::Active);
    c.kind = ConversationKind::AssistantDm {
        assistant_id: AssistantId::from_raw(RawUlid::from_u128(7)),
        context_project: None,
    };
    c.approval_mode = Some(ApprovalMode::FullAccess);
    c.switch_dm_project(Some(p)).unwrap();
    assert_eq!(c.approval_mode, None);
    assert!(
        matches!(c.kind, ConversationKind::AssistantDm { context_project: Some(q), .. } if q == p)
    );

    // 项目对话不能切项目，档位也不动
    let mut project = project_conversation(S::Active);
    project.approval_mode = Some(ApprovalMode::Auto);
    assert_eq!(
        project.switch_dm_project(Some(p)),
        Err(DomainError::NotAssistantDm { id: project.id })
    );
    assert_eq!(project.approval_mode, Some(ApprovalMode::Auto));
}

/// 级联删除：名下有没到终态的回合就拒，并点名对话与回合；都到终态（或没有回合）就过。
#[test]
fn cascade_needs_every_turn_terminal() {
    use iwork_domain::conversation::check_cascade;
    use iwork_domain::id::{ConversationId, RawUlid, TurnId};
    use iwork_domain::machine::Finite;
    use iwork_domain::turn::TurnState;
    let c = ConversationId::from_raw(RawUlid::from_u128(10));
    let t = |n| TurnId::from_raw(RawUlid::from_u128(n));
    assert!(check_cascade(c, &[]).is_ok());
    for state in TurnState::all() {
        let open = !matches!(
            state,
            TurnState::Completed | TurnState::Cancelled | TurnState::Failed
        );
        let r = check_cascade(c, &[(t(1), TurnState::Completed), (t(2), state)]);
        assert_eq!(r.is_err(), open, "{state:?}");
        if let Err(e) = r {
            assert!(e.message.contains(&c.to_string()) && e.message.contains(&t(2).to_string()));
        }
    }
}

/// 已读位置只往前走（T10a 1/F-2）：比库里大才过；相等、更小都拒，并点名对话与两个位置。
#[test]
fn read_marker_only_moves_forward() {
    use iwork_domain::conversation::check_read_committed;
    use iwork_domain::event::ConversationRead;
    let c = conv_id(10);
    let read = |n| ConversationRead {
        conversation_id: c,
        last_read_seq: Seq(n),
    };
    assert!(check_read_committed(Seq(5), &read(6)).is_ok());
    assert!(check_read_committed(Seq(0), &read(1)).is_ok());
    for stale in [5, 1] {
        let e = check_read_committed(Seq(5), &read(stale)).unwrap_err();
        assert!(
            e.message.contains(&c.to_string())
                && e.message.contains(&stale.to_string())
                && e.message.contains('5'),
            "{}",
            e.message
        );
    }
}
