//! 提醒的会话侧接线（T09 方案 2.8）：报在看哪、被叫到前台时跳转、Dock 角标与跳动。
//! 算法分别在 `presence`、`attention` 两个模块；这里只管什么时候算、什么时候发。

use std::time::{Duration, SystemTime};

use tokio::time::Instant;

use iwork_ipc::client::ClientError;
use iwork_proto::pb;

use super::presence::{self, ARRIVE_DELAY, Presence};
use super::{Internal, Session, log_fields};
use crate::session::dial::Dialer;

/// 系统通知被点之后多久之内叫到前台还算数（方案 2.8）。
pub const OPEN_TARGET_FRESH: Duration = Duration::from_secs(30);

impl<D: Dialer> Session<D> {
    // ---------- 报在看哪 ----------

    /// 每轮调：期望值变了就按「离开立刻、到来延后」发。连上后第一次直接发当前值。
    pub(super) fn serve_presence(&mut self, now: Instant) {
        if !self.live() {
            return;
        }
        let want = presence::desired(&self.store, self.window_open);
        let Some(sent) = self.presence_sent.clone() else {
            self.presence_due = None;
            self.send_presence(want);
            return;
        };
        if sent == want {
            self.presence_due = None;
            return;
        }
        let (now_part, later) = presence::split(&sent, &want);
        if let Some(p) = now_part {
            self.send_presence(p);
        }
        if !later {
            self.presence_due = None;
            return;
        }
        match &self.presence_due {
            // 同一个目标在等：到点就发
            Some((at, p)) if *p == want => {
                if *at <= now {
                    self.presence_due = None;
                    self.send_presence(want);
                }
            }
            // 目标又变了：从现在重新等
            _ => self.presence_due = Some((now + ARRIVE_DELAY, want)),
        }
    }

    /// 不进在飞命令表：断了就算了，重新连上后发当前值（方案 2.8）。
    fn send_presence(&mut self, p: Presence) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let command = pb::Command {
            command_id: ulid::Ulid::generate().to_string(),
            kind: Some(pb::command::Kind::ReportPresence(p.to_proto())),
        };
        self.presence_sent = Some(p);
        self.stats.presence_reports += 1;
        tokio::spawn(async move {
            match client.request(pb::request::Body::Command(command)).await {
                Ok(r) => {
                    if let Some(pb::response::Body::Error(e)) = r.body {
                        tracing::warn!(target: "iwork_client", code = %log_fields::error_code_name(e.code), "报在看哪被拒");
                    }
                }
                Err(ClientError::Closed) => {}
                Err(e) => {
                    tracing::warn!(target: "iwork_client", error = %log_fields::client_error(&e), "报在看哪没发出去")
                }
            }
        });
    }

    // ---------- 被系统通知叫到前台 ----------

    /// 每轮调：要问（窗口刚获得焦点、启动后第一次连上）且没在问，就问一次。
    pub(super) fn ask_open_target(&mut self) {
        if !self.live() || !self.open_target_wanted || self.open_target_in_flight {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        self.open_target_wanted = false;
        self.open_target_in_flight = true;
        let tx = self.internal_tx.clone();
        let epoch = self.conn_epoch;
        tokio::spawn(async move {
            let query = pb::Query {
                kind: Some(pb::query::Kind::GetOpenTarget(pb::GetOpenTarget {})),
            };
            let result = match client.request(pb::request::Body::Query(query)).await {
                Ok(r) => match r.body {
                    Some(pb::response::Body::Result(pb::QueryResult {
                        kind: Some(pb::query_result::Kind::GetOpenTarget(t)),
                        ..
                    })) => Some(t),
                    other => {
                        tracing::warn!(target: "iwork_client", response = %log_fields::response(other.as_ref()), "GetOpenTarget 的回包不对");
                        None
                    }
                },
                Err(ClientError::Closed) => None,
                Err(e) => {
                    tracing::warn!(target: "iwork_client", error = %log_fields::client_error(&e), "GetOpenTarget 没问到");
                    None
                }
            };
            let _ = tx.send(Internal::OpenTarget { epoch, result });
        });
    }

    pub(super) fn on_open_target(&mut self, epoch: u64, result: Option<pb::GetOpenTargetResult>) {
        if epoch != self.conn_epoch {
            // 旧连接上的回包：那条连接关闭时在飞标记已清（`drop_connection`），新连接会重新问；它答的可能是换掉
            // 之前的那个后台，拿来比较会把导航和看过的记录改回去（审查 2/F-1）。和别的异步回包入口一样先丢掉
            return;
        }
        self.open_target_in_flight = false;
        let Some(t) = result else {
            return;
        };
        // 比上次处理过的新、而且是 30 秒内点的，才跳（方案 2.8；后台拿不到就不记，不猜）
        if !is_new_open_target(self.open_target_seen.as_ref(), &t) {
            return;
        }
        self.open_target_seen = Some((t.boot_id.clone(), t.token));
        let fresh =
            t.at.as_ref()
                .map(crate::derive::to_system_time)
                .and_then(|at| SystemTime::now().duration_since(at).ok())
                .is_some_and(|age| age <= OPEN_TARGET_FRESH);
        if fresh {
            self.store
                .open_pending_item(&t.pending_item_id, t.conversation_id.as_deref());
        }
    }

    // ---------- Dock 角标与跳动 ----------

    /// 待处理表整份换过之后调（摘要重查的那一条、快照）。
    pub(super) fn pending_replaced(&mut self) {
        for e in self.attention.on_pending_replaced(&self.store) {
            let _ = self.effects.send(e);
        }
    }
}

/// 后台答的目标是不是还没处理过的一次点击。token 只在后台的同一次运行（`boot_id`）里递增，后台重启后从头数，
/// 所以按 (boot_id, token) 比：同一次运行里比上次大才算新；换了一次运行，任何非零 token 都算新。token 为 0 是没点过。
/// 断线重连不清 `seen`：同一个后台重连时答的还是那一个，不能再跳（审查 2026-11-14-m1-t10a-8a 1/F-1）。
pub(super) fn is_new_open_target(
    seen: Option<&(String, u64)>,
    t: &pb::GetOpenTargetResult,
) -> bool {
    if t.token == 0 {
        return false;
    }
    match seen {
        Some((boot, token)) if *boot == t.boot_id => t.token > *token,
        _ => true,
    }
}

#[cfg(test)]
mod open_target_tests {
    use super::*;
    use crate::session::test_support::session;
    use std::time::UNIX_EPOCH;

    fn target(boot: &str, token: u64) -> pb::GetOpenTargetResult {
        pb::GetOpenTargetResult {
            token,
            boot_id: boot.into(),
            ..Default::default()
        }
    }

    #[test]
    fn new_open_target_by_boot_and_token() {
        let seen = ("b1".to_owned(), 3);
        let cases = [
            (None, target("b1", 0), false, "没点过"),
            (None, target("b1", 1), true, "第一次"),
            (
                Some(&seen),
                target("b1", 3),
                false,
                "同一个后台、同一次点击（重连后再问到）",
            ),
            (Some(&seen), target("b1", 2), false, "同一个后台、更早的"),
            (Some(&seen), target("b1", 4), true, "同一个后台、新的一次"),
            (Some(&seen), target("b2", 1), true, "后台重启后的第一次"),
            (Some(&seen), target("b2", 0), false, "后台重启后还没点过"),
        ];
        for (seen, t, want, name) in cases {
            assert_eq!(is_new_open_target(seen, &t), want, "{name}");
        }
    }

    fn clicked(boot: &str, token: u64, conversation: &str) -> pb::GetOpenTargetResult {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        pb::GetOpenTargetResult {
            token,
            boot_id: boot.into(),
            pending_item_id: format!("item-{conversation}"),
            conversation_id: Some(conversation.into()),
            at: Some(prost_types::Timestamp {
                seconds: i64::try_from(now.as_secs()).unwrap(),
                nanos: 0,
            }),
        }
    }

    fn navigated(s: &Session<crate::session::test_support::NoDial>) -> Option<String> {
        s.store.navigate.as_ref().map(|n| n.conversation_id.clone())
    }

    /// 审查 2/F-1：旧连接（旧后台 A）的回包晚于新连接（新后台 B）的回包到，不能把导航和看过的记录改回 A。
    /// 回包经 `on_internal` 注入，和主循环收到它时走同一条路。
    #[tokio::test]
    async fn old_connection_reply_arriving_late_is_dropped() {
        let mut s = session();
        s.conn_epoch = 2;
        s.open_target_in_flight = true;
        s.on_internal(Internal::OpenTarget {
            epoch: 2,
            result: Some(clicked("B", 1, "c-b")),
        });
        assert_eq!(
            navigated(&s).as_deref(),
            Some("c-b"),
            "对照：新连接的回包照跳"
        );
        assert!(!s.open_target_in_flight);
        s.on_internal(Internal::OpenTarget {
            epoch: 1,
            result: Some(clicked("A", 1, "c-a")),
        });
        assert_eq!(navigated(&s).as_deref(), Some("c-b"), "旧连接的回包不跳");
        assert_eq!(
            s.open_target_seen,
            Some(("B".to_owned(), 1)),
            "看过的记录不变"
        );
    }
}
