//! 时间与 ID：domain 不读时钟、不取随机数，由这两个同步端口提供。

use iwork_domain::common::Timestamp;
use iwork_domain::id::RawUlid;

/// 单调时钟读数（纳秒，起点任意）。只用来量时长，不能当日期用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonotonicInstant(pub u64);

/// 时钟。
pub trait Clock: Send + Sync {
    /// 当前 UTC 时间，写进事件与实体。
    fn now(&self) -> Timestamp;
    /// 单调时钟读数，量超时与耗时（系统时间回拨不影响它）。
    fn monotonic(&self) -> MonotonicInstant;
}

/// ULID 生成器。
///
/// **契约：单调**。同一实例先后两次调用，后一个严格大于前一个：同一毫秒内递增随机部分
/// （[`RawUlid::increment`]）；随机部分用尽时进到下一毫秒；系统时间回拨时沿用上一次的毫秒数
/// 继续递增。这样同一毫秒、时钟回拨时按 ID 排序仍等于创建顺序（方案第 8 节 S9）。
/// `tests/fakes.rs` 的 `MonotonicIdGenerator` 是参照实现与用例。
pub trait IdGenerator: Send + Sync {
    fn next(&self) -> RawUlid;
}
