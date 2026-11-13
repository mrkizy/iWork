//! 测试夹具 `Captured` 本身的保证：装上之后，本线程发出的事件一条不漏。
//!
//! 单独放一个测试程序、里面只有这一个测试：同一程序里别的测试也装 `Captured` 时，
//! 竞态被它们的分发器掩盖，这个测试就验不到东西了。

mod support;

use support::Captured;

fn emit() {
    tracing::warn!(event = "capture_probe");
}

/// 另一个没装订阅者的线程先命中同一个 callsite，本线程装的 `Captured` 也得收到这条事件。
/// 没有全局兜底时，tracing 按那个线程（没有订阅者）把 interest 算成 never 并缓存，这条事件被整条丢掉。
#[test]
fn sees_events_after_another_thread_registered_the_callsite() {
    let (audit, _g) = Captured::install();
    std::thread::spawn(emit).join().unwrap();
    emit();
    assert_eq!(audit.events("capture_probe").len(), 1);
}
