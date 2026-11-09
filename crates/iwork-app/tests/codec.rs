//! `ProtoEventCodec` 与「重建后仍能判迟到的审查结果」（方案 4.3 乱序用例 6）。

mod support;

use std::collections::BTreeMap;

use iwork_app::codec::ProtoEventCodec;
use iwork_domain::action::{Action, ActionFlags, ActionKind, ActionTarget, ToolRef};
use iwork_domain::approval::{
    Approval, ApprovalApplyError, ApprovalContext, ApprovalKind, ApprovalState, ApprovalTrigger,
    Decider, ReviewState, StaleReason,
};
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::common::Timestamp;
use iwork_domain::event::{
    ApprovalRequested, ApprovalReviewUpdated, ApprovalStaleResultDropped, DomainEvent, StreamKind,
};
use iwork_domain::id::{ApprovalId, ConversationId, RawUlid, ReviewId, Seq, TurnId};
use iwork_domain::rule::RuleSetVersion;
use iwork_ports::store::{CodecError, EventCodec, Projector, StorageError};
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use support::arb::event_strategies;

/// 经 `EventCodec` 接口编解码等于原值，且编码时的种类就是 `DomainEvent::kind()`。
#[test]
fn proto_event_codec_roundtrip() {
    let codec = ProtoEventCodec;
    assert_eq!(codec.protocol_major(), iwork_proto::PROTOCOL_MAJOR);
    for (kind, strategy) in event_strategies() {
        let mut runner = TestRunner::new(Config {
            cases: 16,
            ..Config::default()
        });
        runner
            .run(&strategy, |event| {
                let encoded = codec
                    .encode(&event)
                    .map_err(|e| TestCaseError::fail(e.to_string()))?;
                if encoded.kind != event.kind() {
                    return Err(TestCaseError::fail(format!(
                        "种类 {} ≠ {}",
                        encoded.kind,
                        event.kind()
                    )));
                }
                let back = codec
                    .decode(encoded.kind, &encoded.payload)
                    .map_err(|e| TestCaseError::fail(e.to_string()))?;
                if back != event {
                    return Err(TestCaseError::fail("往返不等"));
                }
                Ok(())
            })
            .unwrap_or_else(|e| panic!("事件 {kind}：{e}"));
    }
}

#[test]
fn codec_rejects_kind_mismatch_and_garbage() {
    let codec = ProtoEventCodec;
    let event: DomainEvent = iwork_domain::event::RuleRevoked {
        rule_id: iwork_domain::id::RuleId::from_raw(RawUlid::from_u128(1)),
    }
    .into();
    let encoded = codec.encode(&event).unwrap();
    // 对照：种类对得上能解
    assert_eq!(codec.decode("rule_revoked", &encoded.payload), Ok(event));
    assert!(matches!(
        codec.decode("project_removed", &encoded.payload),
        Err(CodecError::Malformed { field, .. }) if field == "type"
    ));
    assert!(matches!(
        codec.decode("rule_revoked", &[0xff, 0xff]),
        Err(CodecError::Malformed { .. })
    ));
}

// ---------- 最小内存 store：只为用例 6，在线提交与重建都经 codec 与同一个投影 ----------

/// 投影表：审批表 + 审计表。
#[derive(Debug, Clone, Default, PartialEq)]
struct Tables {
    approvals: BTreeMap<ApprovalId, Approval>,
    audit: Vec<Seq>,
}

struct ApprovalProjector;

impl Projector for ApprovalProjector {
    type Tx = Tables;
    fn project(&self, tx: &mut Tables, seq: Seq, event: &DomainEvent) -> Result<(), StorageError> {
        let missing = |id: ApprovalId| StorageError::Projection {
            seq,
            message: format!("审批 {id} 不存在"),
        };
        match event {
            DomainEvent::ApprovalRequested(e) => {
                tx.approvals.insert(e.approval.id, e.approval.clone());
            }
            DomainEvent::ApprovalReviewUpdated(e) => {
                tx.approvals
                    .get_mut(&e.approval_id)
                    .ok_or_else(|| missing(e.approval_id))?
                    .review = e.review.clone();
            }
            DomainEvent::ApprovalDecided(e) => {
                tx.approvals
                    .get_mut(&e.approval_id)
                    .ok_or_else(|| missing(e.approval_id))?
                    .state = e.state;
            }
            // audit 流：只追加审计表，不碰审批表
            DomainEvent::ApprovalStaleResultDropped(_) => tx.audit.push(seq),
            _ => {}
        }
        Ok(())
    }
}

struct Row {
    seq: Seq,
    kind: &'static str,
    payload: Vec<u8>,
}

struct MemoryStore {
    codec: ProtoEventCodec,
    projector: ApprovalProjector,
    rows: Vec<Row>,
    tables: Tables,
}

impl MemoryStore {
    fn commit(&mut self, events: Vec<DomainEvent>) {
        for event in events {
            let seq = Seq(self.rows.len() as u64 + 1);
            let encoded = self.codec.encode(&event).expect("能编码");
            self.projector
                .project(&mut self.tables, seq, &event)
                .expect("能投影");
            self.rows.push(Row {
                seq,
                kind: encoded.kind,
                payload: encoded.payload,
            });
        }
    }

    fn rebuild(&mut self) {
        self.tables = Tables::default();
        for row in &self.rows {
            let event = self.codec.decode(row.kind, &row.payload).expect("能解码");
            self.projector
                .project(&mut self.tables, row.seq, &event)
                .expect("能投影");
        }
    }

    /// 某条流上的事件条数（解码后按 `stream()` 归类）。
    fn count_in_stream(&self, kind: StreamKind) -> usize {
        self.rows
            .iter()
            .filter(|r| self.codec.decode(r.kind, &r.payload).unwrap().stream().0 == kind)
            .count()
    }
}

fn review(n: u128) -> ReviewId {
    ReviewId(TurnId::from_raw(RawUlid::from_u128(1000 + n)))
}

fn pending_approval() -> Approval {
    Approval {
        id: ApprovalId::from_raw(RawUlid::from_u128(1)),
        turn_id: TurnId::from_raw(RawUlid::from_u128(2)),
        conversation_id: ConversationId::from_raw(RawUlid::from_u128(3)),
        kind: ApprovalKind::OutsideSandbox,
        action: Action {
            kind: ActionKind::Execute,
            target: ActionTarget::Command {
                program: "npm".into(),
                subcommand: Some("publish".into()),
                argv: vec![],
            },
            tool: ToolRef("Bash".into()),
            raw_summary: "npm publish".into(),
            flags: ActionFlags {
                out_of_bounds: true,
                ..ActionFlags::default()
            },
        },
        matched_rules: vec![],
        ruleset_version: RuleSetVersion("2026.11".into()),
        state: ApprovalState::Pending,
        review: ReviewState::NotReviewed,
        offered_remember: vec![],
        remember_label: String::new(),
        requested_at: Timestamp(1_000),
    }
}

/// 用例 6：审查身份落盘，重建投影后迟到结果的判定与重建前一样。
#[test]
fn approval_review_identity_survives_rebuild() {
    const AUTO: ApprovalContext = ApprovalContext {
        mode_now: ApprovalMode::Auto,
    };
    let (r1, r2) = (review(1), review(2));
    let mut store = MemoryStore {
        codec: ProtoEventCodec,
        projector: ApprovalProjector,
        rows: vec![],
        tables: Tables::default(),
    };

    // 在线：请求审批，AI 审查开始（子状态变化经 apply 得出，再发事件）
    let mut online = pending_approval();
    let id = online.id;
    online
        .apply(ApprovalTrigger::AiReviewStarted { review_id: r1 }, &AUTO)
        .unwrap();
    store.commit(vec![
        ApprovalRequested {
            approval: pending_approval(),
        }
        .into(),
        ApprovalReviewUpdated {
            approval_id: id,
            review: online.review.clone(),
        }
        .into(),
    ]);
    assert_eq!(
        store.tables.approvals[&id], online,
        "对照：在线投影与 apply 结果一致"
    );

    // 清投影、解码重建
    store.rebuild();
    let mut rebuilt = store.tables.approvals[&id].clone();
    assert_eq!(rebuilt.review, ReviewState::InProgress { review_id: r1 });
    assert_eq!(rebuilt, online);

    // 错 id 的「通过」：Stale，审批深比较不变；只写一条 audit 流事件，审批流无新事件
    let before = rebuilt.clone();
    let approval_events_before = store.count_in_stream(StreamKind::Approval);
    let err = rebuilt
        .apply(ApprovalTrigger::AiReviewPassed { review_id: r2 }, &AUTO)
        .unwrap_err();
    let ApprovalApplyError::Stale { review_id, why, .. } = err else {
        panic!("应为 Stale：{err:?}")
    };
    assert_eq!((review_id, why), (r2, StaleReason::ReviewIdMismatch));
    assert_eq!(rebuilt, before);
    store.commit(vec![
        ApprovalStaleResultDropped {
            approval_id: id,
            review_id,
            result: iwork_domain::approval::ReviewPhase::Passed,
            why,
        }
        .into(),
    ]);
    assert_eq!(
        store.count_in_stream(StreamKind::Approval),
        approval_events_before,
        "审批流无新事件"
    );
    assert_eq!(store.count_in_stream(StreamKind::Audit), 1);
    assert_eq!(store.tables.approvals[&id], before, "审批投影不变");
    assert_eq!(store.tables.audit, vec![Seq(3)]);

    // 对号的「通过」在自动档下放行
    rebuilt
        .apply(ApprovalTrigger::AiReviewPassed { review_id: r1 }, &AUTO)
        .unwrap();
    assert_eq!(
        rebuilt.state,
        ApprovalState::Approved {
            decider: Decider::AiReview
        }
    );
}
