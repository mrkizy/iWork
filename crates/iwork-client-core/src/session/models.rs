//! 取模型清单（附带查询 ListModels，见 store::models）：打开的对话用哪个运行时就取哪个的清单；新建助手弹窗
//! 要内置 iWork 那个运行时的（侧栏的 new_assistant_menu）。
//! 结果从回包拿，经 `Internal::ModelsLoaded` 回到主循环；失败按用途 "models" 退避后再取。

use iwork_ipc::client::ClientError;
use iwork_proto::pb;
use tokio::time::Instant;

use super::{Internal, Session, log_fields};
use crate::derive::conversation::menu_runtime;
use crate::derive::sidebar::new_assistant_runtime;
use crate::session::dial::Dialer;

/// 退避表里这种用途的名字。
const PURPOSE: &str = "models";

impl<D: Dialer> Session<D> {
    pub(super) fn want_models(&mut self, now: Instant) {
        if !self.live() || self.backed_off(PURPOSE, now) {
            return;
        }
        // 输入框菜单要的那个，和新建助手弹窗要的那个（内置 iWork 的运行时）；一次取一个，取完下一轮再看
        let Some(runtime_id) = [
            menu_runtime(&self.store),
            new_assistant_runtime(&self.store),
        ]
        .into_iter()
        .flatten()
        .find(|r| self.store.models.wants(r))
        .map(str::to_owned) else {
            return;
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        self.store.models.started(&runtime_id);
        let tx = self.internal_tx.clone();
        let epoch = self.conn_epoch;
        tokio::spawn(async move {
            let query = pb::Query {
                kind: Some(pb::query::Kind::ListModels(pb::ListModels {
                    runtime_id: runtime_id.clone(),
                })),
            };
            let result = match client.request(pb::request::Body::Query(query)).await {
                Ok(r) => match r.body {
                    Some(pb::response::Body::Result(pb::QueryResult {
                        kind: Some(pb::query_result::Kind::ListModels(m)),
                        ..
                    })) => Ok(m),
                    // 原因进日志：只留错误码与回包种类，不留后台的说明文字和回包内容（log_fields）
                    Some(pb::response::Body::Error(e)) => Err(Some(format!(
                        "server:{}",
                        log_fields::error_code_name(e.code)
                    ))),
                    other => Err(Some(format!(
                        "ListModels 的回包不对：{}",
                        log_fields::response(other.as_ref())
                    ))),
                },
                // 连接断了：不算失败，重新 Live 后照常再取。
                Err(ClientError::Closed) => Err(None),
                Err(e) => Err(Some(log_fields::client_error(&e))),
            };
            let _ = tx.send(Internal::ModelsLoaded {
                epoch,
                runtime_id,
                result,
            });
        });
    }

    pub(super) fn on_models(
        &mut self,
        epoch: u64,
        runtime_id: &str,
        result: Result<pb::ListModelsResult, Option<String>>,
    ) {
        if epoch != self.conn_epoch {
            // 旧连接上的结果：那条连接关闭时在取的已作废，新连接会重新取。
            return;
        }
        match result {
            Ok(m) => {
                self.store.set_models(runtime_id, m);
                self.query_succeeded(PURPOSE);
            }
            Err(why) => {
                self.store.models.failed(runtime_id);
                if let Some(why) = why {
                    tracing::warn!(target: "iwork_client", runtime_id, why, "模型清单没取到，退避后再取");
                    self.query_failed(PURPOSE);
                }
            }
        }
    }
}
