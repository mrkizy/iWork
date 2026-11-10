//! 表的归属登记（方案 2.5）。重建只清 [`PROJECTION_TABLES`]，[`DURABLE_TABLES`] 永不清。
//! 加一张表 = 一个新迁移 + 这里一行；单测断言「库里的用户表 = 两张登记表的并集」，漏登记就红。

/// 一张投影表：表名与主键列（逐表对比、导出时按主键排序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionTable {
    pub name: &'static str,
    pub key: &'static [&'static str],
}

const fn t(name: &'static str, key: &'static [&'static str]) -> ProjectionTable {
    ProjectionTable { name, key }
}

/// 可从 `events` 重建的表。
pub const PROJECTION_TABLES: &[ProjectionTable] = &[
    t("projects", &["id"]),
    t("assistants", &["id"]),
    t("assistant_versions", &["assistant_id", "version"]),
    t("conversations", &["id"]),
    t(
        "conversation_member_modes",
        &["conversation_id", "assistant_id"],
    ),
    t("messages", &["id"]),
    t("turns", &["id"]),
    t("turn_usage", &["turn_id"]),
    t("tool_calls", &["turn_id", "tool_call_id"]),
    t("approvals", &["id"]),
    t("user_actions", &["id"]),
    t("agents_md_drafts", &["card_id"]),
    t("approval_rules", &["id"]),
    t("attachments", &["id"]),
    t("runtimes", &["id"]),
    t("quota_snapshots", &["runtime_id", "window"]),
    t("local_config_items", &["runtime_id", "id"]),
    t("global_settings", &["id"]),
    t("artifact_refs", &["area", "sha256", "ref_kind", "ref_id"]),
];

/// 不是投影、重建不清的表：日志与幂等（`meta`、`events`、`commands`、`outbox`），
/// 以及不从事件推导的持久状态（流式检查点、制品对象登记）。
pub const DURABLE_TABLES: &[&str] = &[
    "meta",
    "events",
    "commands",
    "outbox",
    "message_checkpoints",
    "artifact_objects",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_registry_covers_all_tables() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::migrate::apply_all_for_test(&conn);
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_schema WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .unwrap();
        let in_db: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let mut registered: Vec<String> = PROJECTION_TABLES
            .iter()
            .map(|t| t.name)
            .chain(DURABLE_TABLES.iter().copied())
            .map(str::to_owned)
            .collect();
        registered.sort();
        assert!(in_db.len() > 20, "对照：库里确实有表");
        assert_eq!(in_db, registered, "库里的表与登记表不一致");
    }

    /// 每张投影表都有 `updated_seq`，主键列都存在。
    #[test]
    fn projection_tables_have_updated_seq_and_key_columns() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::migrate::apply_all_for_test(&conn);
        for table in PROJECTION_TABLES {
            let cols: Vec<String> = conn
                .prepare(&format!(
                    "SELECT name FROM pragma_table_info('{}')",
                    table.name
                ))
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert!(
                cols.iter().any(|c| c == "updated_seq"),
                "{} 缺 updated_seq",
                table.name
            );
            for k in table.key {
                assert!(cols.iter().any(|c| c == k), "{} 缺主键列 {k}", table.name);
            }
        }
    }
}
