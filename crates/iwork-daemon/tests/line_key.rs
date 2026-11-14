//! 会话线能不能沿用（D8，b45 1/F-3）：判定方持有的 `SessionKeys` 有一样变了就不沿用，只有快照里记审计用的
//! `lists_seq` 例外。换回合模型（AI 审查没单独指定模型时跟着变）、换助手版本，都要换线。

mod support;

use iwork_daemon::host::ports::HostEnv;
use iwork_daemon::host::prepare::{Prepared, prepare};
use iwork_domain::turn::Turn;
use iwork_host::fs::OsPathResolver;
use iwork_ports::read_model::{ReadModelPort, SnapshotInputs};
use support::turns::*;

struct Base {
    env: HostEnv,
    fs: OsPathResolver,
    inputs: SnapshotInputs,
    turn: Turn,
}

impl Base {
    fn prepare(&self, turn: &Turn) -> Prepared {
        prepare(&self.env, &self.fs, &self.inputs, turn).unwrap()
    }
}

async fn base(h: &H) -> Base {
    let id = h.send(1, "hi").await;
    let r = support::fixtures::read_model(&h.raw);
    let (turn, c) = r.turn_with_conversation(id).await.unwrap().unwrap();
    let inputs = r.snapshot_inputs(c.id).await.unwrap();
    let env = HostEnv {
        platform: HostEnv::platform(),
        home: h.env.tmp.path().join("home"),
        data_dir: h.env.dir.root().into(),
        tmp_dirs: vec![],
    };
    let fs = OsPathResolver::new(env.home.clone(), false);
    Base {
        env,
        fs,
        inputs,
        turn,
    }
}

/// 对照：同一个回合算两次，键相等（沿用这条线）。
#[tokio::test]
async fn same_inputs_reuse_the_line() {
    let h = harness(Opts::default()).await;
    let b = base(&h).await;
    assert_eq!(b.prepare(&b.turn).line_key, b.prepare(&b.turn).line_key);
}

#[tokio::test]
async fn model_change_refreshes_decider() {
    let h = harness(Opts::default()).await;
    let b = base(&h).await;
    let a = b.prepare(&b.turn);
    let mut t = b.turn.clone();
    t.model.model_id.0 = "different-model".into();
    let c = b.prepare(&t);
    assert_ne!(
        a.keys.review_model, c.keys.review_model,
        "前提：审查模型跟着变了"
    );
    assert_ne!(a.line_key, c.line_key, "审查模型变了还沿用旧判定方");
}

#[tokio::test]
async fn assistant_version_change_refreshes_decider() {
    let h = harness(Opts::default()).await;
    let b = base(&h).await;
    let a = b.prepare(&b.turn);
    let mut t = b.turn.clone();
    t.assistant_version += 1;
    let c = b.prepare(&t);
    assert_ne!(a.keys.assistant_version, c.keys.assistant_version);
    assert_ne!(a.line_key, c.line_key, "助手版本变了还沿用旧判定方");
}

/// 唯一的例外：快照里记审计用的 `lists_seq` 前进了（清单内容没变），照样沿用这条线。
#[tokio::test]
async fn lists_seq_alone_keeps_the_line() {
    let h = harness(Opts::default()).await;
    let mut b = base(&h).await;
    let a = b.prepare(&b.turn);
    b.inputs.lists_seq = iwork_domain::id::Seq(b.inputs.lists_seq.0 + 7);
    let c = b.prepare(&b.turn);
    assert_ne!(
        a.keys.snapshot.lists_seq, c.keys.snapshot.lists_seq,
        "前提：序号变了"
    );
    assert_eq!(a.line_key, c.line_key);
}
