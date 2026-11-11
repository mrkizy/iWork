//! 各运行时的模型清单（「运行时 · 模型」菜单用，ui-m1.md 第 2 节第 14 条）。
//!
//! `ListModels` 是附带查询（`query_lane` 的 SideQuery）：结果在回包里，不进事件流，所以不经 `Store::apply`。
//! 「还要再取」只看两件事：这个运行时还没取到、也没有在取（方案 2.3 的写法：不留「已经发过」的记号）。
//! 连接断了，在取的一律作废，重新 Live 后再取；取到的留着（清单在一次运行里不会变）。

use std::collections::{BTreeMap, BTreeSet};

use iwork_proto::pb;

#[derive(Debug, Default)]
pub struct ModelCatalog {
    loaded: BTreeMap<String, pb::ListModelsResult>,
    in_flight: BTreeSet<String>,
}

impl ModelCatalog {
    pub fn get(&self, runtime_id: &str) -> Option<&pb::ListModelsResult> {
        self.loaded.get(runtime_id)
    }

    /// 这个运行时要不要去取。
    pub fn wants(&self, runtime_id: &str) -> bool {
        !runtime_id.is_empty()
            && !self.loaded.contains_key(runtime_id)
            && !self.in_flight.contains(runtime_id)
    }

    pub fn started(&mut self, runtime_id: &str) {
        self.in_flight.insert(runtime_id.to_owned());
    }

    pub fn loaded(&mut self, runtime_id: &str, result: pb::ListModelsResult) {
        self.in_flight.remove(runtime_id);
        self.loaded.insert(runtime_id.to_owned(), result);
    }

    /// 取失败：放回「还要再取」，由会话层的退避决定什么时候再取。
    pub fn failed(&mut self, runtime_id: &str) {
        self.in_flight.remove(runtime_id);
    }

    pub fn connection_closed(&mut self) {
        self.in_flight.clear();
    }
}
