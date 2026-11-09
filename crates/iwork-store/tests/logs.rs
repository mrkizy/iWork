//! 卡「注意」：日志不得写入消息正文与附件内容（只写 id 与长度）。
//! 把全部 tracing 输出（含写线程、读线程、I/O 线程）捕获下来，跑一遍带标记正文的各条路径，
//! 断言日志里没有标记串。

mod support;

use std::io::Write;
use std::sync::{Arc, Mutex};

use iwork_domain::common::ArtifactArea;
use iwork_domain::event::*;
use iwork_domain::message::{MessageBody, MessageStatus};
use iwork_ports::artifact::ArtifactStore;
use iwork_ports::read_model::{MessagePage, ReadModelPort};
use iwork_ports::store::StoragePort;
use support::*;

const MARKER: &str = "PAYLOAD_MARKER_6f1d";

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn logs_never_contain_payload() {
    let sink = Captured::default();
    let writer = sink.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let env = Env::new();
    {
        let store = env.open();
        let art = env.artifacts(&store);
        let body = format!("{MARKER} 正文");
        let r = block(art.put_bytes(ArtifactArea::Attachments, body.clone().into_bytes())).unwrap();
        let mut m = user_message(1, 10, &body);
        m.body = MessageBody::Inline(body.clone());
        block(store.commit(cmd(
            1,
            vec![
                ProjectAdded { project: project(1) }.into(),
                ConversationCreated { conversation: project_conversation(10, 1) }.into(),
                TurnQueued { turn: turn(20, 10) }.into(),
                MessageAppended { message: m }.into(),
                MessageAppended { message: assistant_message(2, 10, 20) }.into(),
            ],
        )))
        .unwrap();
        block(store.checkpoint_message(mid(2), 3, &body)).unwrap();
        block(store.commit(cmd(
            2,
            vec![
                MessageCompleted {
                    conversation_id: cid(10),
                    message_id: mid(2),
                    body: MessageBody::Inline(body.clone()),
                    status: MessageStatus::Completed,
                }
                .into(),
            ],
        )))
        .unwrap();
        // 失败路径也要过一遍：投影报错的消息里不能带正文
        let _ = block(store.commit(cmd(
            3,
            vec![MessageAppended { message: user_message(1, 10, &body) }.into()],
        )));
        block(read_model(&store).conversation(
            cid(10),
            MessagePage {
                before: None,
                limit: 10,
            },
        ))
        .unwrap();
        block(store.rebuild_projections()).unwrap();
        block(art.read_range(&r, 0..4)).unwrap();
        env.clock.advance(3 * 24 * 3600 * 1000);
        block(art.release(&r)).ok();
        block(art.collect_garbage()).unwrap();
        block(art.recover()).unwrap();
        store.close().unwrap();
    }
    // 迁移、再打开
    drop(env.open());

    let logs = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
    assert!(!logs.is_empty(), "对照：确实捕获到了日志");
    assert!(
        logs.contains("制品回收完成"),
        "对照：几条路径都打了日志：{logs}"
    );
    assert!(!logs.contains(MARKER), "日志里出现了正文：{logs}");
}
