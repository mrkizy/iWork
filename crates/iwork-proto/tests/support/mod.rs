//! 金样本的登记表与读写工具，`tests/compat.rs` 与 `examples/dump-compat.rs` 共用（例子经 `#[path]` 引入）。
//!
//! 一个样本 = 同名的三份文件：`<短消息名>.<n>.binpb`（二进制，期望的线格式）、`.txtpb`（text format，人读）、
//! `.json`（proto3 canonical JSON，TS 侧 `client/src/gen.test.ts` 用 protobuf-es `toJson` 比对）。

#![allow(dead_code, reason = "测试与例子各用其中一部分")]

pub mod builders;

use std::path::{Path, PathBuf};

use iwork_proto::pb;
use prost::Message;
use prost_reflect::text_format::FormatOptions;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor};

/// 一种有样本的消息：类型名、经生成类型重编码的函数、按编号排列的构造函数。
pub struct SampleType {
    /// `iwork.v1` 包内的短名，如 `Approval`。
    pub name: &'static str,
    /// 用生成的 Rust 类型解码再编码：生成类型认不出的字段会在这一步丢掉，比对时就会红。
    pub reencode: fn(&[u8]) -> Result<Vec<u8>, prost::DecodeError>,
    /// 第 i 个（从 1 起）是 `<name>.<i>` 样本。只许在末尾追加，不许删改、不许调换顺序。
    pub builders: &'static [fn() -> Vec<u8>],
}

fn reencode<T: Message + Default>(bytes: &[u8]) -> Result<Vec<u8>, prost::DecodeError> {
    T::decode(bytes).map(|m| m.encode_to_vec())
}

/// 登记表：加一个样本 = 在对应类型的列表末尾加一个构造函数；加一种类型 = 加一行。
macro_rules! sample_types {
    ($($ty:ident => [$($build:path),* $(,)?]),* $(,)?) => {
        pub const SAMPLE_TYPES: &[SampleType] = &[$(SampleType {
            name: <pb::$ty as prost::Name>::NAME,
            reencode: reencode::<pb::$ty>,
            builders: &[$(|| {
                let build: fn() -> pb::$ty = $build;
                build().encode_to_vec()
            }),*],
        }),*];
    };
}

use builders as b;

sample_types! {
    Hello => [b::hello],
    Welcome => [b::welcome],
    Request => [b::request_command],
    Response => [b::response_error],
    Event => [b::event_persisted],
    Chunk => [b::chunk],
    Subscribe => [b::subscribe],
    Command => [
        b::command_send_message,
        b::command_decide_approval,
        b::command_set_approval_mode,
        b::command_update_project_settings,
        b::command_upload_attachment,
        b::command_execute_user_action,
        b::command_add_security_list_entry,
        b::command_create_assistant,
    ],
    CommandAccepted => [b::command_accepted],
    Query => [b::query_get_conversation],
    QueryResult => [
        b::query_result_get_conversation,
        b::query_result_read_artifact,
        b::query_result_pending,
    ],
    Approval => [b::approval_full],
    Turn => [b::turn_full],
    UserActionCard => [b::user_action_card_full],
    Rule => [b::rule_full],
    RuntimeStatus => [b::runtime_status_full],
    Error => [b::error],
    SessionConstraints => [b::session_constraints],
    SessionToolSet => [b::session_tool_set],
    SessionModel => [b::session_model],
    TransientEvent => [
        b::transient_message_delta,
        b::transient_tool_output_delta,
        b::transient_clone_progress,
        b::transient_subscription_lagging,
    ],
    DomainEvent => [
        b::ev_project_added,
        b::ev_project_updated,
        b::ev_project_relocated,
        b::ev_project_path_status_changed,
        b::ev_project_removed,
        b::ev_project_settings_updated,
        b::ev_repository_initialized,
        b::ev_clone_finished,
        b::ev_agents_md_draft_proposed,
        b::ev_agents_md_draft_decided,
        b::ev_assistant_created,
        b::ev_assistant_updated,
        b::ev_assistant_deleted,
        b::ev_assistant_reset,
        b::ev_conversation_created,
        b::ev_conversation_deleting,
        b::ev_conversation_deleted,
        b::ev_conversation_model_changed,
        b::ev_approval_mode_changed,
        b::ev_dm_project_switched,
        b::ev_conversation_read,
        b::ev_message_appended,
        b::ev_message_completed,
        b::ev_turn_queued,
        b::ev_turn_state_changed,
        b::ev_turn_finished,
        b::ev_turn_model_reported,
        b::ev_tool_call_started,
        b::ev_tool_call_finished,
        b::ev_approval_requested,
        b::ev_approval_review_updated,
        b::ev_approval_decided,
        b::ev_approval_voided,
        b::ev_approval_evaluated,
        b::ev_rule_remembered,
        b::ev_rule_revoked,
        b::ev_rules_deleted,
        b::ev_approval_stale_result_dropped,
        b::ev_user_action_proposed,
        b::ev_user_action_state_changed,
        b::ev_security_list_entry_added,
        b::ev_security_list_entry_removed,
        b::ev_attachment_stored,
        b::ev_attachment_confirmed,
        b::ev_runtime_updated,
        b::ev_quota_observed,
        b::ev_usage_reported,
        b::ev_local_import_prompted,
        b::ev_local_config_imported,
        b::ev_mcp_secret_set,
        b::ev_global_settings_changed,
        b::ev_recovery_completed,
    ],
}

/// 一个样本的三份文件内容。
pub struct SampleFiles {
    pub binpb: Vec<u8>,
    pub txtpb: String,
    pub json: String,
}

/// `proto/compat/`（仓库里的位置，与当前工作目录无关）。
pub fn compat_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../proto/compat")
}

/// 当前主版本的契约目录 `proto/compat/v<N>/`。
pub fn major_dir() -> PathBuf {
    compat_root().join(format!("v{}", iwork_proto::PROTOCOL_MAJOR))
}

pub fn pool() -> DescriptorPool {
    DescriptorPool::decode(iwork_proto::FILE_DESCRIPTOR_SET)
        .expect("FILE_DESCRIPTOR_SET 必须能建描述符池")
}

/// 按短名找描述符：样本文件名只有短名，包名取自当前协议。
pub fn descriptor(pool: &DescriptorPool, short_name: &str) -> MessageDescriptor {
    let full = format!("{}.{short_name}", iwork_proto::PROTO_PACKAGE);
    pool.get_message_by_name(&full)
        .unwrap_or_else(|| panic!("描述符里没有消息 {full}"))
}

/// 由二进制样本生成三份文件。text 与 JSON 都从描述符反射得出，与生成的 Rust 类型无关。
pub fn render(desc: MessageDescriptor, binpb: Vec<u8>) -> SampleFiles {
    let message = DynamicMessage::decode(desc, binpb.as_slice()).expect("样本必须能按描述符解码");
    let txtpb = format!(
        "{}\n",
        message.to_text_format_with_options(&FormatOptions::new().pretty(true))
    );
    // 默认序列化选项即 proto3 canonical JSON：跳过默认值、lowerCamelCase、枚举写名字、64 位整数写字符串，
    // 与 protobuf-es `toJson` 的默认值一致。
    let json = format!(
        "{}\n",
        serde_json::to_string_pretty(&message).expect("样本必须能写成 JSON")
    );
    SampleFiles { binpb, txtpb, json }
}

/// `<name>.<index>` 样本三份文件的路径（binpb、txtpb、json）。
pub fn sample_paths(dir: &Path, name: &str, index: usize) -> [PathBuf; 3] {
    ["binpb", "txtpb", "json"].map(|ext| dir.join(format!("{name}.{index}.{ext}")))
}

/// 已有的样本文件与新生成的内容是否相同。
///
/// `.binpb` 逐字节比（map 已生成为 BTreeMap，编码确定）。`.txtpb`、`.json` 按含义比：
/// prost-reflect 内部用 HashMap 存 map 字段，同一条消息每次写出的 map 条目顺序不同，逐字节比会误判。
pub fn same_content(desc: &MessageDescriptor, path: &Path, existing: &[u8], new: &[u8]) -> bool {
    let ext = path.extension().and_then(|e| e.to_str());
    match ext {
        Some("binpb") => existing == new,
        Some("txtpb") => {
            let parse = |bytes: &[u8]| {
                std::str::from_utf8(bytes)
                    .ok()
                    .and_then(|text| DynamicMessage::parse_text_format(desc.clone(), text).ok())
            };
            matches!((parse(existing), parse(new)), (Some(a), Some(b)) if a == b)
        }
        Some("json") => {
            let parse = |bytes: &[u8]| serde_json::from_slice::<serde_json::Value>(bytes).ok();
            matches!((parse(existing), parse(new)), (Some(a), Some(b)) if a == b)
        }
        _ => panic!("不认识的样本文件 {}", path.display()),
    }
}
