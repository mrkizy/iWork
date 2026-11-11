//! 事件路由与查询分道：后台与客户端都要读的两份协议事实（T03 方案 2.8）。
//!
//! - [`StreamKey`]：订阅范围与事件之间唯一的匹配依据。范围 → 键、事件 → 键两张表在
//!   `iwork-app::subscription`（真值在领域层的 `StreamKind`），`iwork-ipc` 只做覆盖匹配，不解释键的含义。
//!   放在这里是因为 app 与 ipc 都要用它，而 app 不能依赖 ipc、ipc 不能依赖 domain。
//! - [`query_lane`]：哪些查询的结果会写进客户端的视图。服务端据此决定查询走「交接」还是并行查询道，
//!   客户端据此决定结果从 `Incoming` 还是从 future 出来；两端读同一张表。

use std::borrow::Cow;

use crate::pb::query::Kind as QueryKind;

/// 路由键。`kind` 是领域 `StreamKind` 的蛇形名（如 `"conversation"`），ipc 当不透明字符串；
/// `id` 为 `None` 表示整类（范围一侧：覆盖这一类的全部实体；事件一侧：整类一条流，如设置）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamKey {
    pub kind: Cow<'static, str>,
    pub id: Option<String>,
}

impl StreamKey {
    /// 整类键：范围一侧表示「这一类的全部实体」。
    pub const fn all(kind: &'static str) -> Self {
        Self {
            kind: Cow::Borrowed(kind),
            id: None,
        }
    }

    /// 指向一个实体的键。
    pub fn one(kind: &'static str, id: impl Into<String>) -> Self {
        Self {
            kind: Cow::Borrowed(kind),
            id: Some(id.into()),
        }
    }

    /// 范围的键 `self` 是否覆盖事件的路由键 `route`：同类，且 `self` 是整类或 id 相同。
    pub fn covers(&self, route: &StreamKey) -> bool {
        self.kind == route.kind && (self.id.is_none() || self.id == route.id)
    }
}

/// 查询走哪条道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryLane {
    /// 结果会成为某个视图的成员或内容，事件也会改同一份东西：走事件流的交接，
    /// 结果插在流里 `QueryResult.seq` 的位置，客户端按到达顺序应用。
    ViewQuery,
    /// 结果不进视图（制品字节、运行时给的模型列表）：并行执行，结果直接交给发起方。
    SideQuery,
}

/// 查询 → 道的登记表。穷举 `Query.kind`：加一种查询，编译器逼着在这里登记。
pub fn query_lane(kind: &QueryKind) -> QueryLane {
    match kind {
        QueryKind::GetConversation(_)
        | QueryKind::ListConversations(_)
        | QueryKind::ListProjects(_)
        | QueryKind::ListAssistants(_)
        | QueryKind::ListPendingItems(_)
        | QueryKind::GetQuota(_)
        | QueryKind::GetRuntimeStatus(_)
        | QueryKind::ListRules(_)
        | QueryKind::ListLocalConfig(_)
        | QueryKind::ListSecurityLists(_)
        | QueryKind::GetSettings(_)
        | QueryKind::ListAnchors(_) => QueryLane::ViewQuery,
        QueryKind::ReadArtifact(_) | QueryKind::ListModels(_) => QueryLane::SideQuery,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pb;

    #[test]
    fn whole_kind_key_covers_every_id_of_that_kind_only() {
        let all = StreamKey::all("conversation");
        assert!(all.covers(&StreamKey::one("conversation", "c1")));
        assert!(all.covers(&StreamKey::all("conversation")));
        assert!(!all.covers(&StreamKey::one("project", "c1")));
    }

    #[test]
    fn entity_key_covers_same_id_only() {
        let k = StreamKey::one("conversation", "c1");
        assert!(k.covers(&StreamKey::one("conversation", "c1")));
        assert!(!k.covers(&StreamKey::one("conversation", "c2")));
        // 事件是整类一条流（id 为空）时，指向单个实体的范围键不覆盖它。
        assert!(!k.covers(&StreamKey::all("conversation")));
    }

    #[test]
    fn artifact_and_models_are_side_queries_lists_are_view_queries() {
        assert_eq!(
            query_lane(&QueryKind::ReadArtifact(pb::ReadArtifact::default())),
            QueryLane::SideQuery
        );
        assert_eq!(
            query_lane(&QueryKind::ListModels(pb::ListModels::default())),
            QueryLane::SideQuery
        );
        assert_eq!(
            query_lane(&QueryKind::ListAnchors(pb::ListAnchors::default())),
            QueryLane::ViewQuery
        );
        assert_eq!(
            query_lane(&QueryKind::GetConversation(pb::GetConversation::default())),
            QueryLane::ViewQuery
        );
        assert_eq!(
            query_lane(&QueryKind::ListPendingItems(pb::ListPendingItems::default())),
            QueryLane::ViewQuery
        );
    }
}
