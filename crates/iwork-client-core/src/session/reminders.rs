//! 提醒的会话侧接线（T09 方案 2.8）：报在看哪、被叫到前台时跳转、Dock 角标与跳动。
//! 算法分别在 `presence`、`attention` 两个模块；这里只管什么时候算、什么时候发。

use std::time::{Duration, SystemTime};

use tokio::time::Instant;

use iwork_ipc::client::ClientError;
use iwork_proto::pb;

use super::presence::{self, ARRIVE_DELAY, Presence};
use super::{Internal, Session};
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
                        tracing::warn!(target: "iwork_client", code = e.code, message = %e.message, "报在看哪被拒");
                    }
                }
                Err(ClientError::Closed) => {}
                Err(e) => tracing::warn!(target: "iwork_client", error = %e, "报在看哪没发出去"),
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
                        tracing::warn!(target: "iwork_client", ?other, "GetOpenTarget 的回包不对");
                        None
                    }
                },
                Err(ClientError::Closed) => None,
                Err(e) => {
                    tracing::warn!(target: "iwork_client", error = %e, "GetOpenTarget 没问到");
                    None
                }
            };
            let _ = tx.send(Internal::OpenTarget { epoch, result });
        });
    }

    pub(super) fn on_open_target(&mut self, epoch: u64, result: Option<pb::GetOpenTargetResult>) {
        if epoch == self.conn_epoch {
            self.open_target_in_flight = false;
        }
        let Some(t) = result else {
            return;
        };
        // token 只记在内存里：比上次处理过的新、而且是 30 秒内点的，才跳（方案 2.8；后台拿不到就不记，不猜）
        if t.token == 0 || t.token <= self.open_target_seen {
            return;
        }
        self.open_target_seen = t.token;
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
