//! 每个假后台场景：网页逐帧折叠的结果 == 重新 attach 后那一帧全量（Rust 侧的录像对拍）。
//!
//! 守的是「行的视图模型读了行外的状态，那份状态变了行就得重发」与帧引擎的增量规则：漏标一处脏、
//! 漏发一次增量，折叠结果就停在旧值上，和全量对不上。录法是两段式（tests/support/record.rs）：
//! 先打开 focus 对话拿到快照，再跑剩下的事件，所以比的是增量，不是两份快照（审查 2a 1/F-2）；
//! 每个场景都断言打开之后确实收到过增量。

// 假后台只在 Unix 上监听（iwork-fake-daemon）；M1 暂不支持 Windows。
#![cfg(unix)]

mod support;

use std::time::Duration;

use iwork_client_core::Options;
use iwork_fake_daemon::scenario;
use iwork_proto::pb;
use prost::Message;
use support::web::{Web, WebState};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(300);

/// 对拍时只比视图内容：跳转请求是一次性的事件，全量帧不重发它。
fn comparable(s: &WebState) -> WebState {
    WebState {
        navigate: None,
        ..s.clone()
    }
}

fn fold(frames: &[Vec<u8>]) -> WebState {
    let mut web = Web::new(0);
    for f in frames {
        web.apply(&pb::ViewPatch::decode(f.as_slice()).unwrap());
    }
    web.state
}

#[tokio::test(flavor = "multi_thread")]
async fn every_scenario_folds_to_full() {
    let mut mismatched = Vec::new();
    for s in scenario::REGISTRY {
        let rec = timeout(LIMIT, support::record::record(s, Options::default()))
            .await
            .unwrap_or_else(|_| panic!("场景 {} 超时", s.name))
            .unwrap_or_else(|e| panic!("场景 {} 录不下来：{e}", s.name));
        assert!(
            rec.increments > 0,
            "{}：打开之后没有收到任何增量帧，对拍会退化成比两份快照",
            s.name
        );
        let folded = comparable(&fold(&rec.frames));
        let full = comparable(&fold(std::slice::from_ref(&rec.full)));
        // 对照：真比了东西
        assert!(full.sidebar.is_some(), "{}：全量帧带侧栏", s.name);
        if s.focus.is_some() {
            assert!(!full.rows.is_empty(), "{}：全量帧带对话行", s.name);
        }
        if folded != full {
            let diff: Vec<_> = full
                .rows
                .iter()
                .filter(|(k, v)| folded.rows.get(*k) != Some(*v))
                .map(|(k, _)| k.clone())
                .collect();
            // 行以外的视图哪几份对不上（调试时一眼看出是哪个视图漏了增量）
            let views: Vec<&str> = [
                ("sidebar", folded.sidebar != full.sidebar),
                ("pending", folded.pending != full.pending),
                ("settings", folded.settings != full.settings),
                ("assistant", folded.assistant != full.assistant),
                ("add_project", folded.add_project != full.add_project),
                ("connection", folded.connection != full.connection),
                ("list", folded.list != full.list),
                ("header", folded.header != full.header),
                ("composer", folded.composer != full.composer),
                ("home", folded.home != full.home),
                ("anchors", folded.anchors != full.anchors),
            ]
            .into_iter()
            .filter_map(|(n, differ)| differ.then_some(n))
            .collect();
            mismatched.push(format!(
                "{}（对不上的行：{diff:?}；对不上的视图：{views:?}）",
                s.name
            ));
        }
    }
    assert!(mismatched.is_empty(), "逐帧折叠与全量不等：{mismatched:#?}");
}
