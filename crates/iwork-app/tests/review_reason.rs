use iwork_app::convert::{review_state_from_proto, review_state_to_proto};
use iwork_domain::id::{RawUlid, TurnId};
use iwork_proto::pb;

#[test]
fn passed_and_failed_review_reasons_survive_wire_roundtrip() {
    let id = TurnId::from_raw(RawUlid::from_u128(99)).to_string();
    for phase in [pb::ReviewState::Passed, pb::ReviewState::Failed] {
        let reason = "动作是完成用户任务所必需";
        let state = review_state_from_proto(
            "ApprovalReviewUpdated",
            phase as i32,
            Some(&id),
            Some(reason),
        )
        .unwrap();
        let (encoded_phase, encoded_id, encoded_reason) = review_state_to_proto(&state);
        assert_eq!(encoded_phase, phase as i32);
        assert_eq!(encoded_id.as_deref(), Some(id.as_str()));
        assert_eq!(encoded_reason.as_deref(), Some(reason));
    }
}

#[test]
fn old_passed_review_without_reason_still_decodes() {
    let id = TurnId::from_raw(RawUlid::from_u128(100)).to_string();
    let state = review_state_from_proto(
        "ApprovalReviewUpdated",
        pb::ReviewState::Passed as i32,
        Some(&id),
        None,
    )
    .unwrap();
    assert_eq!(
        review_state_to_proto(&state).0,
        pb::ReviewState::Passed as i32
    );
}
