//! 丢掉一条会话线之前先了结它身上的东西（T10a 方案 D8a；7.2 第 ② 条）。
//!
//! 扩展 `rebind_commit_failure_is_retried_at_next_entry`：补记旧编号的那一笔**一直**写不进去（存储持续回
//! Busy）。这期间照 D8a 的次序换线（`settle` → `retire` → 两样都成了才查最后一条绑定建新线），断言旧线还在、
//! 没有建新线、没有任何会话带着新编号被打开；放开存储后补记先落盘，新线带的是旧编号。
//!
//! 换线本身是回合宿主的事（步 4，`iwork-daemon`）；这里用一个照方案写的小函数扮宿主，验的是 `SessionLine` 给出的
//! 三个入口（`settle`、`retire`、`is_settled`）足够让宿主守住这条规矩。

use super::*;

/// 这个对话最后一条会话绑定的编号（D3a 的查询，这里直接读假存储）。
fn last_binding(store: &MemStore) -> Option<String> {
    store.all().into_iter().rev().find_map(|e| match e {
        DomainEvent::TurnSessionBound(b) => Some(b.runtime_session_id),
        _ => None,
    })
}

/// 扮宿主换线（D8a）：补完绑定回退、确认进程树清空，两样都成了才按最后一条绑定建新线；有一样没成，旧线还回去。
async fn swap_line(
    w: &Arc<W<Reference>>,
    mut line: Line<Reference>,
) -> Result<Line<Reference>, Line<Reference>> {
    let ports = TurnPorts {
        store: &w.store,
        clock: &w.clock,
        ids: &w.ids,
        live: &NoLive,
        timer: &w.timer,
        history: &w.history,
    };
    if line.settle(ports).await.is_err() || line.retire().await.is_err() || !line.is_settled() {
        return Err(line);
    }
    Ok(SessionLine::new(w.decider.clone(), last_binding(&w.store)))
}

#[tokio::test(flavor = "current_thread")]
async fn pending_rebind_blocks_line_swap() {
    let turn = queued_turn(1);
    let w = W::<Reference>::new(&turn).await;
    w.history.set(history());
    w.ctl
        .open_errors
        .lock()
        .unwrap()
        .push_back(lost(SessionLostReason::NotFound));
    w.ctl.fresh_ids.lock().unwrap().push_back(NEW_ID.into());
    w.ctl
        .prompt_errors
        .lock()
        .unwrap()
        .push_back(AgentError::Protocol {
            message: "发不出去".into(),
        });
    // 这一轮依次提交：绑定 + 进 Running、失败收尾、补记旧编号。第三笔起存储一直忙
    let at = w.store.commits.load(Ordering::SeqCst) + 3;
    w.store.fail_commit_at.store(at, Ordering::SeqCst);
    let (result, line) = w
        .spawn(
            SessionLine::new(w.decider.clone(), Some(OLD_ID.into())),
            w.plan(turn),
        )
        .await
        .unwrap();
    assert!(result.is_err(), "补记那一笔没写进去：{result:?}");
    assert!(!line.is_settled(), "补记欠着");
    assert_eq!(
        last_binding(&w.store).as_deref(),
        Some(NEW_ID),
        "对照：库里最后一条是新编号"
    );

    // 下一回合开始前清单变了，要换线；补记那一笔持续写不进去
    w.store.fail_all.store(true, Ordering::SeqCst);
    let opens = w.ctl.opens.load(Ordering::SeqCst);
    let mut line = line;
    for _ in 0..3 {
        line = match swap_line(&w, line).await {
            Ok(_) => panic!("补记没落盘时不该换出新线"),
            Err(kept) => kept,
        };
        assert!(!line.is_settled(), "旧线还欠着补记");
        assert_eq!(line.resume_id(), Some(OLD_ID), "旧线还在");
        assert_eq!(
            w.ctl.opens.load(Ordering::SeqCst),
            opens,
            "这期间没有开任何会话"
        );
        assert_eq!(last_binding(&w.store).as_deref(), Some(NEW_ID));
    }

    // 放开存储：补记先落盘，新线带旧编号
    w.store.fail_all.store(false, Ordering::SeqCst);
    let fresh = swap_line(&w, line).await.ok().expect("补记落盘后可以换线");
    assert_eq!(
        last_binding(&w.store).as_deref(),
        Some(OLD_ID),
        "补记先落盘"
    );
    assert_eq!(fresh.resume_id(), Some(OLD_ID), "新线接着旧编号说");

    let next = queued_turn_seeded(&w, 2);
    let task = w.spawn(fresh, w.plan(next));
    wait_until("prompt", || w.ctl.prompts.load(Ordering::SeqCst) == 1).await;
    w.ctl.send(AgentUpdate::Finished(TurnOutcome::Completed));
    assert_eq!(task.await.unwrap().0.unwrap(), TurnState::Completed);
    let resumes: Vec<_> = w
        .ctl
        .specs
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.resume.clone())
        .collect();
    assert_eq!(
        resumes,
        [Some(OLD_ID.into()), None, Some(OLD_ID.into())],
        "没有哪个会话带着新编号被打开"
    );
}
