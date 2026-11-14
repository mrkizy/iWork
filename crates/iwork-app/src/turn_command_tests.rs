use super::*;
use iwork_domain::approval_mode::ProjectDefaultMode;
use iwork_domain::assistant::{AssistantConfig, AssistantVersion, Avatar};
use iwork_domain::common::{ModelId, ModelSelection, ToolGroups};
use iwork_domain::conversation::ConversationState;
use iwork_domain::id::{AssistantId, ConversationId, ProjectId, RawUlid, RuntimeId, Seq};
use iwork_domain::machine::Finite;
use iwork_domain::project::{PathStatus, ProjectSettings, VcsKind};
use iwork_domain::turn::InterruptReason;

fn id(n: u128) -> RawUlid {
    RawUlid::from_u128(n)
}

fn model(name: &str) -> ModelSelection {
    ModelSelection {
        model_id: ModelId(name.into()),
        effort: None,
    }
}

const PROJECT: u128 = 7;
const IWORK: u128 = 30;
const HELPER: u128 = 31;

fn project_conversation(state: ConversationState) -> Conversation {
    Conversation {
        id: ConversationId::from_raw(id(1)),
        kind: ConversationKind::Project {
            project_id: ProjectId::from_raw(id(PROJECT)),
        },
        state,
        last_read_seq: Seq(0),
        model_override: None,
        approval_mode: None,
        updated_at: Timestamp(0),
    }
}

fn assistant(n: u128, builtin: bool, model_name: &str) -> Assistant {
    Assistant {
        id: AssistantId::from_raw(id(n)),
        builtin,
        current: AssistantVersion {
            version: 3,
            config: AssistantConfig {
                name: format!("a{n}"),
                avatar: Avatar {
                    initial: "a".into(),
                    color: "red".into(),
                },
                prompt: String::new(),
                runtime_id: RuntimeId::from_raw(id(90)),
                model: model(model_name),
                disabled_groups: ToolGroups::default(),
            },
            created_at: Timestamp(0),
        },
    }
}

fn project(default_model: Option<&str>) -> Project {
    Project {
        id: ProjectId::from_raw(id(PROJECT)),
        root_path: "/p".into(),
        vcs_kind: VcsKind("git".into()),
        name: "p".into(),
        pinned: false,
        last_active_at: None,
        path_status: PathStatus::Ok,
        settings: ProjectSettings {
            default_approval_mode: ProjectDefaultMode::Manual,
            default_model: default_model.map(model),
            ai_review: Default::default(),
        },
    }
}

fn turn(n: u128, state: TurnState) -> Turn {
    Turn {
        id: TurnId::from_raw(id(n)),
        conversation_id: ConversationId::from_raw(id(1)),
        assistant_id: AssistantId::from_raw(id(IWORK)),
        assistant_version: 1,
        runtime_id: RuntimeId::from_raw(id(90)),
        model: model("m"),
        runtime_session_id: None,
        kind: TurnKind::Assistant,
        state,
        started_at: Timestamp(0),
        ended_at: None,
    }
}

fn send(
    c: &Conversation,
    assistants: &[Assistant],
    projects: &[Project],
    text: &str,
) -> Result<Vec<DomainEvent>, Refusal> {
    plan_send_message(SendInput {
        conversation: c,
        assistants,
        projects,
        text: text.into(),
        message_id: MessageId::from_raw(id(50)),
        turn_id: TurnId::from_raw(id(51)),
        now: Timestamp(9),
    })
}

fn queued_turn(events: &[DomainEvent]) -> &Turn {
    match events {
        [DomainEvent::MessageAppended(m), DomainEvent::TurnQueued(q)] => {
            assert_eq!(m.message.author, Author::User);
            assert_eq!(m.message.body, MessageBody::Inline("hi".into()));
            &q.turn
        }
        other => panic!("要用户消息与 TurnQueued 两条、在同一笔里：{other:?}"),
    }
}

/// A3：用户消息与 `TurnQueued` 同一笔；项目对话交给内置 iWork，记下它的版本与运行时。
#[test]
fn send_message_queues_turn_for_builtin_assistant() {
    let c = project_conversation(ConversationState::Active);
    let assistants = [
        assistant(HELPER, false, "helper"),
        assistant(IWORK, true, "own"),
    ];
    let events = send(&c, &assistants, &[project(None)], "hi").unwrap();
    let t = queued_turn(&events);
    assert_eq!(t.assistant_id, AssistantId::from_raw(id(IWORK)));
    assert_eq!(t.assistant_version, 3);
    assert_eq!(t.state, TurnState::Queued);
    assert_eq!(
        t.model,
        model("own"),
        "没有改选、项目没设默认：用助手自己的"
    );
}

/// 技术设计 9.6：对话改选 > 项目默认（只对 iWork）> 助手默认。三格各自独立算期望值。
#[test]
fn send_message_model_precedence() {
    let assistants = [assistant(IWORK, true, "own")];
    let mut c = project_conversation(ConversationState::Active);
    let with_default = [project(Some("proj"))];
    assert_eq!(
        queued_turn(&send(&c, &assistants, &with_default, "hi").unwrap()).model,
        model("proj")
    );
    c.model_override = Some(model("override"));
    assert_eq!(
        queued_turn(&send(&c, &assistants, &with_default, "hi").unwrap()).model,
        model("override")
    );
}

#[test]
fn send_message_refusals() {
    let assistants = [assistant(IWORK, true, "own")];
    let deleting = project_conversation(ConversationState::Deleting);
    assert_eq!(
        send(&deleting, &assistants, &[], "hi").unwrap_err().code(),
        iwork_proto::pb::ErrorCode::ConversationDeleting
    );
    let active = project_conversation(ConversationState::Active);
    assert!(matches!(
        send(&active, &[], &[], "hi").unwrap_err(),
        Refusal::NotFound {
            entity: "assistant",
            ..
        }
    ));
    assert!(matches!(
        send(&active, &assistants, &[], "  ").unwrap_err(),
        Refusal::InvalidArgument { .. }
    ));
}

fn every_state() -> Vec<TurnState> {
    TurnState::all()
}

/// A5 的表逐个状态核对 `CancelTurn`：期望值按表手写，不调被测函数里的分支。
#[test]
fn cancel_turn_follows_the_table() {
    let c = project_conversation(ConversationState::Active);
    for state in every_state() {
        let t = turn(5, state);
        let got = plan_cancel_turn(&t, &c);
        match state {
            TurnState::Queued => {
                let events = got.unwrap();
                assert!(matches!(&events[..], [
                    DomainEvent::TurnStateChanged(s),
                    DomainEvent::TurnFinished(f),
                ] if s.from == TurnState::Queued && s.to == TurnState::Cancelled && f.state == TurnState::Cancelled));
            }
            TurnState::Running | TurnState::WaitingApproval => {
                assert!(
                    matches!(&got.unwrap()[..], [DomainEvent::TurnIntentRecorded(i)] if i.intent == TurnIntentKind::Cancel)
                );
            }
            TurnState::Interrupted { .. } => {
                assert_eq!(
                    got.unwrap_err().code(),
                    iwork_proto::pb::ErrorCode::TurnNotCancellable
                );
            }
            TurnState::Cancelling
            | TurnState::Completed
            | TurnState::Cancelled
            | TurnState::Failed => {
                assert_eq!(got.unwrap(), Vec::new(), "{state:?}：空的一笔");
            }
        }
    }
}

#[test]
fn resume_and_abandon_only_on_interrupted() {
    let c = project_conversation(ConversationState::Active);
    for state in every_state() {
        let t = turn(5, state);
        let interrupted = matches!(state, TurnState::Interrupted { .. });
        let resume = plan_resume_turn(&t, &c);
        let used = iwork_domain::common::Usage {
            input_tokens: 7,
            ..Default::default()
        };
        let abandon = plan_abandon_turn(&t, &c, used);
        if interrupted {
            assert!(
                matches!(&resume.unwrap()[..], [DomainEvent::TurnIntentRecorded(i)] if i.intent == TurnIntentKind::Resume)
            );
            // 结束事件带中断前记下的用量累计（用量口径 b）
            assert!(matches!(&abandon.unwrap()[..], [
                DomainEvent::TurnStateChanged(s), DomainEvent::TurnFinished(f)
            ] if s.from == state && s.to == TurnState::Cancelled && f.usage == used));
        } else {
            assert_eq!(
                resume.unwrap_err().code(),
                iwork_proto::pb::ErrorCode::TurnNotResumable,
                "{state:?}"
            );
            assert_eq!(
                abandon.unwrap_err().code(),
                iwork_proto::pb::ErrorCode::TurnNotResumable,
                "{state:?}"
            );
        }
    }
}

/// `CancelAll`：排着的撤回、在跑的落意图，合成一笔；已中断、取消中的不动；别的对话的回合不在里面。
#[test]
fn cancel_all_covers_queued_and_running_in_one_batch() {
    let c = project_conversation(ConversationState::Active);
    let mut other = turn(9, TurnState::Queued);
    other.conversation_id = ConversationId::from_raw(id(2));
    let open = [
        turn(5, TurnState::Running),
        turn(6, TurnState::Queued),
        turn(
            7,
            TurnState::Interrupted {
                reason: InterruptReason::Crash,
            },
        ),
        turn(8, TurnState::Cancelling),
        other,
    ];
    let events = plan_cancel_all(&c, &open).unwrap();
    let touched: Vec<(TurnId, &str)> = events
        .iter()
        .map(|e| match e {
            DomainEvent::TurnIntentRecorded(i) => (i.turn_id, "intent"),
            DomainEvent::TurnStateChanged(s) => (s.turn_id, "changed"),
            DomainEvent::TurnFinished(f) => (f.turn_id, "finished"),
            other => panic!("{other:?}"),
        })
        .collect();
    let t = |n| TurnId::from_raw(id(n));
    assert_eq!(
        touched,
        [(t(5), "intent"), (t(6), "changed"), (t(6), "finished")]
    );
}
