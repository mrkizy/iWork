//! 方案 2.10 对拍：投影器从事件里提取的制品引用，与「把事件编成 proto 后递归扫出全部 `ArtifactRef`
//! 消息」（独立算法，不与 `artifact_refs_in` 共用代码）逐个相等。proto 里出现了新的带引用字段而
//! 投影器没跟上时，这里会红。

#[path = "../../iwork-app/tests/support/arb.rs"]
#[allow(dead_code)]
mod arb;

use iwork_app::codec::ProtoEventCodec;
use iwork_domain::event::DomainEvent;
use iwork_ports::store::EventCodec;
use iwork_store::artifact_refs_in;
use std::cell::Cell;

use proptest::test_runner::{Config, TestCaseError, TestRunner};
use prost_reflect::{DescriptorPool, DynamicMessage, ReflectMessage, Value};

/// `(sha256 十六进制, 区的 proto 枚举名, 大小)`。
type Found = (String, String, u64);

fn scan(msg: &DynamicMessage, out: &mut Vec<Found>) {
    if msg.descriptor().full_name() == "iwork.v1.ArtifactRef" {
        let area_desc = msg.descriptor().get_field_by_name("area").unwrap();
        let area_num = msg.get_field(&area_desc).as_enum_number().unwrap();
        let area = area_desc
            .kind()
            .as_enum()
            .unwrap()
            .get_value(area_num)
            .unwrap()
            .name()
            .to_owned();
        out.push((
            msg.get_field_by_name("sha256")
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned(),
            area,
            msg.get_field_by_name("size").unwrap().as_u64().unwrap(),
        ));
        return;
    }
    for (_, value) in msg.fields() {
        scan_value(value, out);
    }
}

fn scan_value(value: &Value, out: &mut Vec<Found>) {
    match value {
        Value::Message(m) => scan(m, out),
        Value::List(items) => items.iter().for_each(|v| scan_value(v, out)),
        Value::Map(map) => map.values().for_each(|v| scan_value(v, out)),
        _ => {}
    }
}

fn from_proto(pool: &DescriptorPool, event: &DomainEvent) -> Vec<Found> {
    let bytes = ProtoEventCodec.encode(event).unwrap().payload;
    let desc = pool.get_message_by_name("iwork.v1.DomainEvent").unwrap();
    let msg = DynamicMessage::decode(desc, bytes.as_slice()).unwrap();
    let mut out = Vec::new();
    scan(&msg, &mut out);
    out.sort();
    out
}

fn from_projector(event: &DomainEvent) -> Vec<Found> {
    let mut out: Vec<Found> = artifact_refs_in(event)
        .into_iter()
        .map(|r| {
            let area = match r.area {
                iwork_domain::common::ArtifactArea::Artifacts => "ARTIFACT_AREA_ARTIFACTS",
                iwork_domain::common::ArtifactArea::Attachments => "ARTIFACT_AREA_ATTACHMENTS",
                iwork_domain::common::ArtifactArea::Private => "ARTIFACT_AREA_PRIVATE",
            };
            (r.sha256.to_string(), area.to_owned(), r.size)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn artifact_refs_match_arbitrary_events() {
    let pool = DescriptorPool::decode(iwork_proto::FILE_DESCRIPTOR_SET).unwrap();
    let total_refs = Cell::new(0usize);
    let strategies = arb::event_strategies();
    assert_eq!(
        strategies.len(),
        DomainEvent::KINDS.len(),
        "生成器覆盖全部事件"
    );
    for (kind, strategy) in strategies {
        let mut runner = TestRunner::new(Config {
            cases: 32,
            ..Config::default()
        });
        runner
            .run(&strategy, |event| {
                let expected = from_proto(&pool, &event);
                let got = from_projector(&event);
                total_refs.set(total_refs.get() + expected.len());
                if expected != got {
                    return Err(TestCaseError::fail(format!(
                        "proto 里 {expected:?}，投影器提取 {got:?}"
                    )));
                }
                Ok(())
            })
            .unwrap_or_else(|e| panic!("事件 {kind}：{e}"));
    }
    assert!(
        total_refs.get() > 50,
        "对照：确实比到了引用（{}）",
        total_refs.get()
    );
}
