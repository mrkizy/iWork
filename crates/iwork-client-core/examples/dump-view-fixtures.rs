//! 录像工具（T09 方案 2.6）：对每个场景起假后台 + 真 client-core，把网页会收到的帧写下来。
//!
//! `cargo run -p iwork-client-core --example dump-view-fixtures -- <输出目录> [场景名…]`
//!
//! 每个场景写两个文件，再写一份目录 `index.json`：
//! - `<场景>.frames.binpb`：第一代里收齐的每一帧（编码后的 `ViewPatch`），按 protobuf 长度前缀依次拼接；
//! - `<场景>.full.binpb`：场景跑完、帧取空之后重新 `attach` 得到的那一帧全量（同样带长度前缀，只有一帧）。
//!
//! 网页测试把 `frames` 逐帧 `applyPatch` 折叠，结果应当等于只应用 `full` 的结果。
//! 两段式（与 scenario_fold 测试同一份，tests/support/record.rs）：客户端先连上；跑到建出 focus 对话，
//! 打开它、等快照到；再跑剩下的事件，这一段的帧是对话的增量（流式、断线重连都在里面）。
//! 录像不入库，测试前由 `pnpm -C client fixtures` 生成。
//!
//! 另有几份「合成录像」（只给预览页看，见 [`unix::synthetic`]）：假后台造不出来的连接状态（版本不一致、正在切换版本、
//! 不支持的环境）与「完全退出」确认框。做法是在 `empty` 场景的录像后面接一帧连接视图，视图由 client-core 自己的
//! `derive::connection` 算，文字与出口和真界面同一份。

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    unix::main().await
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    // 假后台只在 Unix 上监听；M1 不支持 Windows（T03），网页的录像对拍测试在 Windows 上跳过。
    eprintln!("dump-view-fixtures：假后台只在 Unix 上可用，不生成录像");
    std::process::ExitCode::SUCCESS
}

// 录法与 scenario_fold 测试同一份（两段式：先打开 focus 对话拿到快照，再跑剩下的事件）。
#[cfg(unix)]
#[path = "../tests/support/record.rs"]
mod record;

#[cfg(unix)]
mod unix {
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::time::Duration;

    use iwork_client_core::Options;
    use iwork_fake_daemon::scenario::{self, Scenario};
    use iwork_proto::pb;
    use tokio::time::timeout;

    use super::record;

    /// 单个场景的总时限：超过就报错点名，不写半截录像。
    const SCENARIO_LIMIT: Duration = Duration::from_secs(300);

    pub async fn main() -> ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let Some((out, names)) = args.split_first() else {
            eprintln!("用法：dump-view-fixtures <输出目录> [场景名…]");
            return ExitCode::from(2);
        };
        let out = PathBuf::from(out);
        let picked: Vec<&'static Scenario> = if names.is_empty() {
            scenario::REGISTRY.iter().collect()
        } else {
            let mut v = Vec::new();
            for n in names {
                match scenario::get(n) {
                    Some(s) => v.push(s),
                    None => {
                        eprintln!("没有这个场景：{n}");
                        return ExitCode::from(2);
                    }
                }
            }
            v
        };
        if let Err(e) = std::fs::create_dir_all(&out) {
            eprintln!("建不了输出目录 {}：{e}", out.display());
            return ExitCode::FAILURE;
        }
        let mut index: Vec<Entry> = Vec::new();
        let mut empty: Option<record::Recording> = None;
        let mut unloaded: Option<record::Recording> = None;
        for s in picked {
            match timeout(SCENARIO_LIMIT, record::record(s, Options::default())).await {
                Ok(Ok(rec)) => {
                    if let Err(e) = write_recording(&out, s.name, &rec) {
                        eprintln!("场景 {} 写不下来：{e}", s.name);
                        return ExitCode::FAILURE;
                    }
                    eprintln!(
                        "{}：{} 帧，其中增量 {} 帧",
                        s.name,
                        rec.frames.len(),
                        rec.increments
                    );
                    index.push(Entry {
                        name: s.name.to_owned(),
                        about: s.about.to_owned(),
                        focus: s.focus,
                        opened: rec.opened,
                    });
                    if s.name == "empty" {
                        empty = Some(rec);
                    } else if s.name == "anchors-unloaded" {
                        unloaded = Some(rec);
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("场景 {} 录不下来：{e}", s.name);
                    return ExitCode::FAILURE;
                }
                Err(_) => {
                    eprintln!("场景 {} 超过 {SCENARIO_LIMIT:?} 还没录完", s.name);
                    return ExitCode::FAILURE;
                }
            }
        }
        // 只在录全部场景时出合成录像（单录某几个时 index.json 只列它们，见 pnpm -C client fixtures）
        if names.is_empty()
            && let Some(base) = &empty
        {
            for s in synthetic() {
                let rec = with_connection(base, &s.connection);
                if let Err(e) = write_recording(&out, s.name, &rec) {
                    eprintln!("合成录像 {} 写不下来：{e}", s.name);
                    return ExitCode::FAILURE;
                }
                index.push(Entry {
                    name: s.name.to_owned(),
                    about: s.about.to_owned(),
                    focus: None,
                    opened: rec.opened,
                });
            }
        }
        // 跳到没载入的一处、那一段还在取：假后台跑完就落地了，录不到这一刻，在 anchors-unloaded 后面接一帧
        if names.is_empty()
            && let Some(base) = &unloaded
        {
            let name = "anchors-jump-loading";
            let rec = with_anchor_seeking(base);
            if let Err(e) = write_recording(&out, name, &rec) {
                eprintln!("合成录像 {name} 写不下来：{e}");
                return ExitCode::FAILURE;
            }
            index.push(Entry {
                name: name.to_owned(),
                about: "消息锚点：点了没载入的最旧一处、那一段还在取（被点的刻度流光、顶端进度线、读屏句）；展开导轨看刻度"
                    .to_owned(),
                focus: Some("c-anchors-long"),
                opened: rec.opened,
            });
        }
        if let Err(e) = std::fs::write(out.join("index.json"), index_json(&index)) {
            eprintln!("写不了 index.json：{e}");
            return ExitCode::FAILURE;
        }
        ExitCode::SUCCESS
    }

    /// 两个文件，都是长度前缀拼接的 ViewPatch：`<场景>.frames.binpb`（第一代逐帧）、`<场景>.full.binpb`（一帧全量）。
    fn write_recording(out: &Path, name: &str, rec: &record::Recording) -> Result<(), String> {
        let mut buf = Vec::new();
        for f in &rec.frames {
            prost::encoding::encode_varint(f.len() as u64, &mut buf);
            buf.extend_from_slice(f);
        }
        write(&out.join(format!("{name}.frames.binpb")), &buf)?;
        let mut one = Vec::new();
        prost::encoding::encode_varint(rec.full.len() as u64, &mut one);
        one.extend_from_slice(&rec.full);
        write(&out.join(format!("{name}.full.binpb")), &one)
    }

    /// index.json 的一项。
    struct Entry {
        name: String,
        about: String,
        focus: Option<&'static str>,
        opened: usize,
    }

    /// 一份合成录像：名字、说明、连接状态。
    pub struct Synthetic {
        pub name: &'static str,
        pub about: &'static str,
        pub connection: pb::ConnectionView,
    }

    /// 假后台造不出来的连接状态（只给预览页看）。视图由 client-core 的 derive::connection 算。
    pub fn synthetic() -> Vec<Synthetic> {
        use iwork_client_core::derive::connection::connection;
        use iwork_client_core::store::{Connection, Store};
        use iwork_ipc::client::WaitPrompt;
        let view = |c: Connection, quit: bool| {
            let mut s = Store::default();
            s.set_connection(c);
            s.set_quit_requested(quit);
            connection(&s)
        };
        vec![
            Synthetic {
                name: "connection-version-mismatch",
                about: "连接类整页：后台服务与客户端版本不一致（说明 + 出口「重新打开 iWork」，不转圈）",
                connection: view(Connection::Waiting(WaitPrompt::VersionMismatch), false),
            },
            Synthetic {
                name: "connection-switching-version",
                about: "连接类整页：正在切换版本（中性提示，后台照常重连）",
                connection: view(Connection::Waiting(WaitPrompt::SwitchingVersion), false),
            },
            Synthetic {
                name: "connection-environment",
                about: "连接类整页：不支持的环境（M1 的 Windows），有标题、没有出口。真界面的说明文字取启动错误原文",
                connection: view(Connection::Environment("M1 暂不支持 Windows".into()), false),
            },
            Synthetic {
                name: "quit-confirm",
                about: "「完全退出 iWork」确认框（菜单点了之后）",
                connection: view(Connection::Live, true),
            },
        ]
    }

    /// 在锚点录像后面接一帧：锚点视图里正在跳第一处（jump_loading 与 jump_loading_key，同 derive::anchors 给的）。
    /// 接的是全量里那份对话 patch 的副本、只改锚点：网页按整份 patch 覆盖标题、输入框等，只带锚点会把它们清空。
    fn with_anchor_seeking(base: &record::Recording) -> record::Recording {
        use prost::Message as _;
        let full = pb::ViewPatch::decode(base.full.as_slice()).expect("自己写的全量帧解得开");
        let mut patch = full
            .changes
            .iter()
            .find_map(|c| match &c.change {
                Some(pb::view_change::Change::Conversation(p)) if p.anchors.is_some() => {
                    Some(p.clone())
                }
                _ => None,
            })
            .expect("anchors-unloaded 的全量帧里有带锚点的对话 patch");
        let anchors = patch.anchors.as_mut().expect("上面挑的就是带锚点的");
        anchors.jump_loading =
            Some(iwork_client_core::derive::text::ANCHOR_JUMP_LOADING.to_owned());
        anchors.jump_loading_key = anchors
            .items
            .first()
            .map(|i| i.key.clone())
            .unwrap_or_default();
        append_change(
            base,
            pb::ViewChange {
                change: Some(pb::view_change::Change::Conversation(patch)),
            },
        )
    }

    /// 在一份录像后面接一帧连接视图；全量帧里同样追加（网页按顺序应用，后来的盖住前面的）。
    fn with_connection(base: &record::Recording, view: &pb::ConnectionView) -> record::Recording {
        append_change(
            base,
            pb::ViewChange {
                change: Some(pb::view_change::Change::Connection(pb::ConnectionPatch {
                    view: Some(view.clone()),
                })),
            },
        )
    }

    /// 在一份录像后面接一帧（一个变化）；全量帧里同样追加（网页按顺序应用，后来的盖住前面的）。
    fn append_change(base: &record::Recording, change: pb::ViewChange) -> record::Recording {
        use prost::Message as _;
        let patch = pb::ViewPatch {
            changes: vec![change.clone()],
            ..Default::default()
        };
        let mut full = pb::ViewPatch::decode(base.full.as_slice()).expect("自己写的全量帧解得开");
        full.changes.push(change);
        let mut frames = base.frames.clone();
        frames.push(patch.encode_to_vec());
        record::Recording {
            frames,
            full: full.encode_to_vec(),
            ..base.clone()
        }
    }

    /// 目录：网页的预览页与测试按它找录像。手写 JSON，免得为一个小文件加依赖。
    fn index_json(list: &[Entry]) -> String {
        let esc = |s: &str| {
            s.chars()
                .flat_map(|c| match c {
                    '"' => "\\\"".chars().collect::<Vec<_>>(),
                    '\\' => "\\\\".chars().collect(),
                    '\n' => "\\n".chars().collect(),
                    c => vec![c],
                })
                .collect::<String>()
        };
        let items: Vec<String> = list
            .iter()
            .map(|e| {
                let focus = e
                    .focus
                    .map_or("null".to_owned(), |f| format!("\"{}\"", esc(f)));
                format!(
                    "  {{\"name\": \"{}\", \"about\": \"{}\", \"focus\": {focus}, \"opened\": {}}}",
                    esc(&e.name),
                    esc(&e.about),
                    e.opened
                )
            })
            .collect();
        format!("[\n{}\n]\n", items.join(",\n"))
    }

    fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
        std::fs::write(path, bytes).map_err(|e| format!("写不了 {}：{e}", path.display()))
    }
}
