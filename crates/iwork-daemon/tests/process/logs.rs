//! 卡验收 6（S12）：日志里检索不到消息正文、凭据、完整环境变量。库里预置带哨兵串的消息正文（恢复会把它写进
//! `MessageCompleted` 并发布），环境里放带哨兵的变量，再经 ipc 发一串带哨兵正文的请求；跑完恢复、发布、停机后
//! grep 整个 `logs/`。

use std::time::Duration;

use iwork_proto::pb;

use crate::fixtures::*;
use crate::seed::{self, STREAMING_MESSAGE};
use crate::support::{Sandbox, code, rt, signal};

const BODY_CANARY: &str = "CANARY-BODY-7f3a91";
const ENV_CANARY: &str = "CANARY-ENV-c41e08";
const TOKEN_CANARY: &str = "sk-ant-CANARY-TOKEN-2b77";
const REQUEST_CANARY: &str = "CANARY-REQUEST-91dd0c";

#[test]
fn logs_contain_no_canaries() {
    let sb = Sandbox::new();
    seed::seed_crash_state(&sb);
    // 检查点正文里放哨兵（恢复时它成为消息的完整正文）
    let store = seed::open(&sb);
    let text = format!("回复里有 {BODY_CANARY}");
    block(iwork_ports::store::StoragePort::checkpoint_message(
        &store,
        mid(STREAMING_MESSAGE),
        text.len() as u64,
        &text,
    ))
    .unwrap();
    store.close().unwrap();

    let mut d = sb.spawn(
        &[],
        &[
            ("IWORK_TEST_CANARY", ENV_CANARY),
            ("ANTHROPIC_API_KEY", TOKEN_CANARY),
            ("IWORK_LOG", "debug"),
        ],
    );
    sb.wait_ready(d.pid(), Duration::from_secs(30));
    rt().block_on(async {
        let (client, _incoming) = sb.connect().await.expect("握手");
        for _ in 0..3 {
            let _ = client
                .request(pb::request::Body::Command(pb::Command {
                    command_id: ulid::Ulid::generate().to_string(),
                    kind: Some(pb::command::Kind::SendMessage(pb::SendMessage {
                        conversation_id: cid(10).to_string(),
                        text: format!("请处理 {REQUEST_CANARY}"),
                        attachment_ids: vec![],
                    })),
                }))
                .await;
        }
    });
    signal(d.pid(), libc::SIGTERM);
    assert_eq!(code(d.wait_exit(Duration::from_secs(30))), Some(0));

    // 对照：哨兵确实走过了这条路（正文进了库，恢复把它写进了事件）
    let events = seed::events_after(&sb, 0);
    assert!(
        events
            .iter()
            .any(|e| format!("{:?}", e.event).contains(BODY_CANARY)),
        "对照：消息正文带着哨兵进了事件"
    );
    let logs_dir = sb.data_root().join("logs");
    let mut all = String::new();
    for entry in std::fs::read_dir(&logs_dir).unwrap() {
        all.push_str(&std::fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    assert!(
        all.contains("后台服务就绪"),
        "对照：日志有内容（debug 级别）"
    );
    assert!(
        all.contains("SendMessage"),
        "对照：请求进了日志（只记种类）"
    );
    for canary in [BODY_CANARY, ENV_CANARY, TOKEN_CANARY, REQUEST_CANARY] {
        assert!(!all.contains(canary), "日志里有哨兵 {canary}");
    }
}
