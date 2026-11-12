//! 场景登记表本身：有 focus 的场景都能在建出 focus 对话处切开，而且切开以后还有改变客户端所见的步骤
//! （事件，或断线、停止监听；对拍要比增量）。

use iwork_fake_daemon::scenario::{self, Step};

#[test]
fn every_focus_scenario_splits_with_events_after_opening() {
    for s in scenario::REGISTRY {
        let (before, after) = s.split_at_focus();
        let appended_after = after
            .iter()
            .filter(|x| {
                matches!(
                    x,
                    Step::Append(_)
                        | Step::Transient(_)
                        | Step::StopListening
                        | Step::DisconnectForSnapshot
                )
            })
            .count();
        match s.focus {
            Some(_) => {
                assert!(!before.is_empty(), "{}：前一段至少有建对话那一步", s.name);
                assert!(
                    appended_after > 0,
                    "{}：打开对话以后客户端看不到任何变化，对拍比不出增量",
                    s.name
                );
            }
            // 没有 focus 时整段在后面；场景用 OpenHere 标了切点的除外（先布置、再做客户端动作，如添加项目）
            None => assert!(
                before.is_empty() || s.steps().iter().any(|x| matches!(x, Step::OpenHere)),
                "{}：没有 focus、也没有 OpenHere 时整段在后面",
                s.name
            ),
        }
    }
}
