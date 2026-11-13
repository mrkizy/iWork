//! 对外错误 `Error.details` 的键，集中在这里，不在各处手写字符串。
//! details 里禁止放凭据（方案 3.4）。

/// 出错的文件或目录路径。
pub const PATH: &str = "path";
/// 写入携带的期望版本（`CONFLICT` 时）。
pub const EXPECTED_VERSION: &str = "expected_version";
/// 解不出的 proto 消息名（转换失败时）。
pub const KIND: &str = "kind";
/// 解不出的字段名（转换失败时）。
pub const FIELD: &str = "field";
/// 握手失败时客户端的协议主版本。真值在 `iwork_proto::handshake`，这里只引用。
pub const CLIENT_MAJOR: &str = iwork_proto::handshake::DETAIL_CLIENT_MAJOR;
/// 握手失败时后台服务的协议主版本。
pub const SERVER_MAJOR: &str = iwork_proto::handshake::DETAIL_SERVER_MAJOR;
/// 完全退出被拒（要确认）时：持有进程的回合数。
pub const RUNNING_TURNS: &str = "running_turns";
/// 完全退出被拒（要确认）时：执行中的亲手执行卡片数。
pub const EXECUTING_CARDS: &str = "executing_cards";
/// 完全退出被拒（要确认）时：排着的回合数（退出会把它们取消，T10a P2 a）。
pub const QUEUED_TURNS: &str = "queued_turns";
/// 完全退出被拒（要确认）时：数目读不出来（`"true"` 时别的数不可信）。
pub const COUNTS_UNKNOWN: &str = "counts_unknown";
