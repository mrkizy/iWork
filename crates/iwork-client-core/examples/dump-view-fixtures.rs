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
        let mut index = Vec::new();
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
                    index.push((s, rec.opened));
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

    /// 目录：网页的预览页与测试按它找录像。手写 JSON，免得为一个小文件加依赖。
    fn index_json(list: &[(&Scenario, usize)]) -> String {
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
            .map(|(s, opened)| {
                let focus = s
                    .focus
                    .map_or("null".to_owned(), |f| format!("\"{}\"", esc(f)));
                format!(
                    "  {{\"name\": \"{}\", \"about\": \"{}\", \"focus\": {focus}, \"opened\": {opened}}}",
                    esc(s.name),
                    esc(s.about)
                )
            })
            .collect();
        format!("[\n{}\n]\n", items.join(",\n"))
    }

    fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
        std::fs::write(path, bytes).map_err(|e| format!("写不了 {}：{e}", path.display()))
    }
}
