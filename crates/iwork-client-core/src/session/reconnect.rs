//! 重连决策（T09 方案 2.3）：纯函数。`from_seq` 只在 [`ReconnectState::from_seq`] 一处算。

use iwork_ipc::client::ConnectError;

use super::dial::DialError;
use crate::store::Connection;

/// 连接关闭计数（与 T03 2.11 的说法逐字对应）：关闭时游标与上一次关闭时相同就加一，不同就置 1；
/// 应用了任何带 seq 的项（事件、快照、查询结果）就清零。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconnectState {
    same_cursor_closes: u32,
    last_close_cursor: Option<u64>,
}

/// 同游标被关到这个次数就改拿快照。
pub const SNAPSHOT_AFTER: u32 = 2;
/// 同游标被关到这个次数就提示「后台负载过高」。
pub const OVERLOAD_AFTER: u32 = 3;

impl ReconnectState {
    pub fn on_closed(&mut self, cursor: u64) {
        if self.last_close_cursor == Some(cursor) && self.same_cursor_closes > 0 {
            self.same_cursor_closes += 1;
        } else {
            self.same_cursor_closes = 1;
        }
        self.last_close_cursor = Some(cursor);
    }

    pub fn on_applied_seq(&mut self) {
        self.same_cursor_closes = 0;
    }

    pub fn same_cursor_closes(&self) -> u32 {
        self.same_cursor_closes
    }

    /// 游标为 0、有视图还是 `Loading`、或同游标已被关两次 → 0（拿快照）；否则续传。
    pub fn from_seq(&self, cursor: u64, any_loading: bool) -> u64 {
        if cursor == 0 || any_loading || self.same_cursor_closes >= SNAPSHOT_AFTER {
            0
        } else {
            cursor
        }
    }

    pub fn overloaded(&self) -> bool {
        self.same_cursor_closes >= OVERLOAD_AFTER
    }
}

/// 连上后又被关：第一次立即重拨；同一游标又被关（续传没送出任何东西就断，多半是后台过载）就按退避等，
/// 退避由调用方在真有进展（应用了带 seq 的项）时复位（审查 1/F-4）。
pub fn close_retry_delay(
    same_cursor_closes: u32,
    backoff: &mut iwork_ipc::client::Backoff,
) -> std::time::Duration {
    if same_cursor_closes <= 1 {
        std::time::Duration::ZERO
    } else {
        backoff.next_delay()
    }
}

/// 拨号失败之后怎么办。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retry {
    /// 按退避重拨。
    Backoff,
    /// 不自动重拨，只留「重试」。
    Manual,
}

/// 拨号失败 → 界面与下一步（方案 2.3 的表）。
pub fn on_dial_error(e: &DialError) -> (Connection, Retry) {
    match e {
        DialError::Connect(c) => match c.wait_prompt() {
            Some(p) => (
                Connection::Waiting {
                    title: p.text(),
                    exits: p.exits().to_vec(),
                },
                Retry::Backoff,
            ),
            None => (Connection::Diagnostics(diagnostics_for(c)), Retry::Backoff),
        },
        DialError::NotReady(d) => (Connection::Diagnostics(d.clone()), Retry::Backoff),
        DialError::Environment(msg) => (Connection::Environment(msg.clone()), Retry::Manual),
    }
}

fn diagnostics_for(e: &ConnectError) -> crate::store::Diagnostics {
    crate::store::Diagnostics {
        reason: e.to_string(),
        lines: Vec::new(),
        newer_schema: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 第 1 次关：续传；第 2 次：快照；第 3 次：提示；应用了一项：清零。
    #[test]
    fn same_cursor_closes_table() {
        let mut r = ReconnectState::default();
        r.on_closed(10);
        assert_eq!(r.from_seq(10, false), 10);
        assert!(!r.overloaded());
        r.on_closed(10);
        assert_eq!(r.from_seq(10, false), 0);
        assert!(!r.overloaded());
        r.on_closed(10);
        assert_eq!(r.from_seq(10, false), 0);
        assert!(r.overloaded());
        r.on_applied_seq();
        assert!(!r.overloaded());
        r.on_closed(11);
        assert_eq!(r.from_seq(11, false), 11);
        // 游标变了：重新从 1 数。
        r.on_closed(12);
        assert_eq!(r.same_cursor_closes(), 1);
    }

    /// 审查 1/F-4：同游标反复被关时重拨间隔按退避增长，不是每次立即。
    #[test]
    fn repeated_closes_back_off() {
        use iwork_ipc::client::Backoff;
        use std::time::Duration;
        let mut b = Backoff::new();
        let mut r = ReconnectState::default();
        let mut delays = Vec::new();
        for _ in 0..5 {
            r.on_closed(10);
            delays.push(close_retry_delay(r.same_cursor_closes(), &mut b));
        }
        assert_eq!(delays[0], Duration::ZERO, "第一次立即重拨");
        assert_eq!(delays[1], Duration::from_millis(200));
        assert!(
            delays.windows(2).skip(1).all(|w| w[1] > w[0]),
            "之后逐次变长：{delays:?}"
        );
    }

    /// R2：有视图还在 Loading 时一律拿快照；游标为 0 也是。
    #[test]
    fn loading_view_forces_snapshot() {
        let r = ReconnectState::default();
        assert_eq!(r.from_seq(10, true), 0);
        assert_eq!(r.from_seq(0, false), 0);
        assert_eq!(r.from_seq(10, false), 10, "对照：都齐了才续传");
    }

    #[test]
    fn dial_error_table() {
        let (c, retry) = on_dial_error(&DialError::Connect(ConnectError::ClosedBeforeWelcome));
        assert!(matches!(
            c,
            Connection::Waiting {
                title: "后台服务与客户端版本不一致",
                ..
            }
        ));
        assert_eq!(retry, Retry::Backoff);
        let (c, _) = on_dial_error(&DialError::Connect(ConnectError::VersionMismatch {
            client_major: 1,
            server_major: 2,
        }));
        assert!(matches!(
            c,
            Connection::Waiting {
                title: "正在切换版本",
                ..
            }
        ));
        let (c, retry) = on_dial_error(&DialError::Connect(ConnectError::Timeout));
        assert!(matches!(c, Connection::Diagnostics(_)));
        assert_eq!(retry, Retry::Backoff);
        let (c, retry) = on_dial_error(&DialError::Environment("M1 暂不支持 Windows".into()));
        assert_eq!(c, Connection::Environment("M1 暂不支持 Windows".into()));
        assert_eq!(retry, Retry::Manual);
    }
}
