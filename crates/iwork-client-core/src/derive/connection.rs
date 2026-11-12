//! 连接类：诊断页、正在切换版本、版本不一致中性提示、重连细条、过载提示（方案 2.3 的表）。

use iwork_proto::pb;

use super::text;
use crate::store::{Connection, Exit, Store};

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
        Connection::Waiting { title, exits } => {
            v.state = pb::ConnectionState::Waiting.into();
            v.title = (*title).into();
            v.exit_actions = exits.iter().map(|e| exit(*e)).collect();
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
            v.message.clone_from(msg);
            v.exit_actions = vec![exit(Exit::Retry)];
        }
    }
    v
}

fn exit(e: Exit) -> pb::ConnectionExit {
    let (label, kind) = match e {
        Exit::Retry => (text::EXIT_RETRY, pb::ConnectionExitKind::Retry),
        Exit::Relaunch => (text::EXIT_RELAUNCH, pb::ConnectionExitKind::Relaunch),
    };
    pb::ConnectionExit {
        label: label.into(),
        kind: kind.into(),
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

    /// 出口带动作（网页按 kind 接，不按文字猜）：版本不一致给「重新打开 iWork」，环境说明给「重试」，
    /// 正常连着时没有出口。
    #[test]
    fn exits_carry_kinds() {
        assert_eq!(
            exits(Connection::Waiting {
                title: "后台服务与客户端版本不一致",
                exits: vec![Exit::Relaunch],
            }),
            vec![(
                "重新打开 iWork".to_owned(),
                pb::ConnectionExitKind::Relaunch
            )]
        );
        assert_eq!(
            exits(Connection::Environment("M1 暂不支持 Windows".into())),
            vec![("重试".to_owned(), pb::ConnectionExitKind::Retry)]
        );
        assert_eq!(exits(Connection::Live), vec![]);
    }
}
