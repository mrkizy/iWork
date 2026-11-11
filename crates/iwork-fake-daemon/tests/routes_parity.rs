//! 假后台的主路由键与 `iwork_app::subscription::routes_for` 的第一个键对拍：拿全部持久事件金样本，
//! 能转成领域事件的逐条比。会话副键不比（G5 落地前两边本来就不同，见 `routes.rs` 文件头）。

use std::path::PathBuf;

use iwork_app::convert::event_from_proto;
use iwork_app::subscription::routes_for;
use iwork_fake_daemon::routes::primary;
use iwork_proto::pb;
use prost::Message;

#[test]
fn primary_route_matches_routes_for_on_every_event_sample() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../proto/compat/v1/samples");
    let mut compared = 0;
    for entry in std::fs::read_dir(&dir).expect("样本目录存在") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !(name.starts_with("DomainEvent.") && name.ends_with(".binpb")) {
            continue;
        }
        let proto = pb::DomainEvent::decode(std::fs::read(&path).unwrap().as_slice())
            .unwrap_or_else(|e| panic!("{name}：{e}"));
        let ours = primary(proto.event.as_ref().expect("样本有载荷"));
        let domain = event_from_proto(proto).unwrap_or_else(|e| panic!("{name}：{e}"));
        assert_eq!(ours, routes_for(&domain)[0], "{name} 的主路由键不一致");
        compared += 1;
    }
    // 对照：真的比了全部种类（52 种事件，每种至少一个样本）。
    assert!(compared >= 52, "只比了 {compared} 个样本");
}
