//! `StoragePort::commit` 的「调用返回时已经入队」契约与排在写队列里的查回执（T10a 方案 2.8 第 0 步、A2）。
//! 放在 crate 内部：要用测试钩子（压住写线程、查回执入队的通知），钩子只在测试构建里有。

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use iwork_domain::event::{DomainEvent, ProjectAdded};
use iwork_ports::store::{CommitReceipt, StoragePort};

use crate::fixtures::*;

fn added(n: u128) -> DomainEvent {
    ProjectAdded {
        project: project(n),
    }
    .into()
}

const LIMIT: Duration = Duration::from_secs(10);

/// 调 `commit` 之后不推进返回的 future、直接丢掉，这一笔照样落盘。查回执排在它后面，查得到。
#[test]
fn commit_lands_without_polling_its_future() {
    let env = Env::new();
    let store = env.open();
    let park = store.park_writer().unwrap();
    park.wait_parked(LIMIT);
    drop(store.commit(cmd(1, vec![added(1)])));
    park.release();
    let receipt = block(store.command_receipt(cmd_id(1))).unwrap();
    assert_eq!(
        receipt,
        Some(CommitReceipt {
            first_seq: iwork_domain::id::Seq(1),
            last_seq: iwork_domain::id::Seq(1),
            replayed: true,
        }),
        "丢掉 future 的那一笔落了盘"
    );
    let names: Vec<String> = block(iwork_ports::read_model::ReadModelPort::projects(
        &read_model(&store),
    ))
    .unwrap()
    .into_iter()
    .map(|p| p.project.name)
    .collect();
    assert_eq!(names, vec!["p1"]);
}

/// 查回执和落盘的先后由写队列定：先入队的那一笔，查回执一定看得到。
enum Seen {
    Enqueued,
    Answered(Option<CommitReceipt>),
}

/// 写线程被压住时，第一笔提交已入队、调用方已放手；随后查同一个 `command_id` 的回执：
/// 查回执入队（测试通知）→ 放开写线程 → 那一笔先落盘、查回执后执行，回 `replayed`。
/// 坏实现（查回执走读连接）在写线程放开之前就回答了「没有」：测试先收到答复而不是入队通知，红。
#[test]
fn receipt_query_is_queued_behind_enqueued_commit() {
    let env = Env::new();
    let store = Arc::new(env.open());
    let (tx, rx) = mpsc::channel::<Seen>();
    let notify = tx.clone();
    store.on_receipt_enqueued(move |_| {
        let _ = notify.send(Seen::Enqueued);
    });
    let park = store.park_writer().unwrap();
    park.wait_parked(LIMIT);
    // 第一份：入队后被放手（受理函数被中止）
    drop(store.commit(cmd(1, vec![added(1)])));
    // 第二份查回执，在另一个线程里等答复
    let asker = {
        let store = store.clone();
        std::thread::spawn(move || {
            let r = block(store.command_receipt(cmd_id(1))).unwrap();
            let _ = tx.send(Seen::Answered(r));
        })
    };
    match rx.recv_timeout(LIMIT).expect("等到入队通知或答复") {
        Seen::Enqueued => {}
        Seen::Answered(r) => panic!("写线程还压着，查回执就回答了 {r:?}：它没有排在写队列里"),
    }
    park.release();
    let answer = match rx.recv_timeout(LIMIT).expect("等到答复") {
        Seen::Answered(r) => r,
        Seen::Enqueued => panic!("只查了一次"),
    };
    asker.join().unwrap();
    let first = block(store.command_receipt(cmd_id(1))).unwrap();
    assert!(first.is_some(), "对照：那一笔确实落了盘");
    assert_eq!(answer, first, "查到的是那一笔的回执");
    // 没提交过的 command_id：None
    assert_eq!(block(store.command_receipt(cmd_id(2))).unwrap(), None);
}
