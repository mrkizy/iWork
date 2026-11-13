//! 连接类：诊断页、正在切换版本、版本不一致中性提示、重连细条、过载提示（方案 2.3 的表）。

use iwork_proto::pb;

use super::text;
use iwork_ipc::client::WaitPrompt;

use crate::store::{Connection, Store};

pub fn connection(store: &Store) -> pb::ConnectionView {
    let mut v = pb::ConnectionView {
        overload_notice: store.overloaded.then(|| text::OVERLOADED.to_owned()),
        commands_enabled: store.connection == Connection::Live,
        quit_requested: store.quit_requested,
        query_notice: store.query_failing.then(|| text::QUERY_RETRYING.to_owned()),
        ..Default::default()
    };
    match &store.connection {
        Connection::Live => v.state = pb::ConnectionState::Live.into(),
        Connection::Connecting => {
            v.state = pb::ConnectionState::Connecting.into();
            v.title = text::CONNECTING.into();
        }
        Connection::Reconnecting => {
            v.state = pb::ConnectionState::Reconnecting.into();
            v.title = text::RECONNECTING.into();
        }
        Connection::Waiting(p) => {
            v.state = pb::ConnectionState::Waiting.into();
            v.title = p.text().into();
            // 版本不一致：说清楚是怎么回事、点了会怎样；只给有实现的出口（查看诊断等 T04p）。
            // 正在切换版本：后台照常退避重连，没有出口（网页据此画转圈）。
            match p {
                WaitPrompt::VersionMismatch => {
                    v.message = text::VERSION_MISMATCH_MESSAGE.into();
                    v.exit_actions = vec![relaunch()];
                }
                WaitPrompt::SwitchingVersion => {}
            }
        }
        Connection::Diagnostics(d) => {
            v.state = pb::ConnectionState::Diagnostics.into();
            v.title = text::DIAGNOSTICS_TITLE.into();
            v.message.clone_from(&d.reason);
            v.diagnostics = Some(pb::DiagnosticsView {
                lines: d
                    .lines
                    .iter()
                    .map(|(l, val)| pb::DiagnosticLine {
                        label: l.clone(),
                        value: val.clone(),
                    })
                    .collect(),
                hint: d.newer_schema.then(|| text::NEWER_SCHEMA_HINT.to_owned()),
                can_retry: true,
            });
        }
        Connection::Environment(msg) => {
            v.state = pb::ConnectionState::Environment.into();
            v.title = text::ENVIRONMENT_TITLE.into();
            v.message.clone_from(msg);
        }
    }
    v
}

fn relaunch() -> pb::ConnectionExit {
    pb::ConnectionExit {
        label: text::EXIT_RELAUNCH.into(),
        kind: pb::ConnectionExitKind::Relaunch.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exits(c: Connection) -> Vec<(String, pb::ConnectionExitKind)> {
        let mut s = Store::default();
        s.set_connection(c);
        connection(&s)
            .exit_actions
            .into_iter()
            .map(|e| (e.label, pb::ConnectionExitKind::try_from(e.kind).unwrap()))
            .collect()
    }

    /// 出口带动作（网页按 kind 接，不按文字猜）：版本不一致给「重新打开 iWork」并说明；正在切换版本、环境说明没有出口；
    /// 环境说明有标题。
    #[test]
    fn exits_carry_kinds() {
        assert_eq!(
            exits(Connection::Waiting(WaitPrompt::VersionMismatch)),
            vec![(
                "重新打开 iWork".to_owned(),
                pb::ConnectionExitKind::Relaunch
            )]
        );
        assert_eq!(
            exits(Connection::Waiting(WaitPrompt::SwitchingVersion)),
            vec![]
        );
        assert_eq!(
            exits(Connection::Environment("M1 暂不支持 Windows".into())),
            vec![]
        );
        assert_eq!(exits(Connection::Live), vec![]);
        let mut s = Store::default();
        s.set_connection(Connection::Waiting(WaitPrompt::VersionMismatch));
        // ui-m1 2a 第 17 条：两种成因都说到（新装了新版 → 重新打开；装回旧版 → 后台重启好自己连上），
        // 被打断的回合点「继续」可接着做；不许说回合不受影响（升级时后台自己重启，回合按中断处理）。
        let message = connection(&s).message;
        assert_eq!(
            message,
            "这个窗口和后台服务不是同一版，暂时连不上。通常是刚装了新版：重新打开 iWork 就会用上新装的那一版；\
             刚装回旧版的话，等后台服务重启好就会自动连上。后台服务仍在运行，重启时被打断的回合可以点「继续」接着做，\
             待你批准的事项都还在。"
        );
        for banned in ["不会因此中断", "不受影响"] {
            assert!(!message.contains(banned), "版本不一致页不许说「{banned}」：{message}");
        }
        s.set_connection(Connection::Environment("M1 暂不支持 Windows".into()));
        let v = connection(&s);
        assert_eq!(
            (v.title.as_str(), v.message.as_str()),
            (text::ENVIRONMENT_TITLE, "M1 暂不支持 Windows")
        );
    }
}
