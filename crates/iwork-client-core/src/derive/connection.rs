//! 连接类：诊断页、正在切换版本、版本不一致中性提示、重连细条、过载提示（方案 2.3 的表）。

use iwork_proto::pb;

use super::text;
use crate::store::{Connection, Store};

pub fn connection(store: &Store) -> pb::ConnectionView {
    let mut v = pb::ConnectionView {
        overload_notice: store.overloaded.then(|| text::OVERLOADED.to_owned()),
        commands_enabled: store.connection == Connection::Live,
        quit_requested: store.quit_requested,
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
            v.exits = exits.iter().map(|e| (*e).to_owned()).collect();
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
            v.exits = vec!["重试".into()];
        }
    }
    v
}
