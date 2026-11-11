//! 录像工具（T09 方案 2.6）：对每个场景起假后台 + 真 client-core，把网页会收到的帧写下来。
//!
//! `cargo run -p iwork-client-core --example dump-view-fixtures -- <输出目录> [场景名…]`
//!
//! 每个场景写两个文件，再写一份目录 `index.json`：
//! - `<场景>.frames.binpb`：第一代里收齐的每一帧（编码后的 `ViewPatch`），按 protobuf 长度前缀依次拼接；
//! - `<场景>.full.binpb`：场景跑完、帧取空之后重新 `attach` 得到的那一帧全量（同样带长度前缀，只有一帧）。
//!
//! 网页测试把 `frames` 逐帧 `applyPatch` 折叠，结果应当等于只应用 `full` 的结果。
//! 场景与客户端同时跑（流式增量、断线重连都会出现在帧里）；场景的 `focus` 对话在假后台里建出来之后才声明打开它。
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

#[cfg(unix)]
mod unix {
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::time::Duration;

    use iwork_client_core::{App, DirectDialer, Options};
    use iwork_fake_daemon::scenario::{self, Scenario};
    use iwork_fake_daemon::{FakeDaemon, self_trusting_identity};
    use iwork_proto::pb;
    use pb::ui_intent::Kind as I;
    use prost::Message;
    use tokio::time::timeout;

    /// 场景跑完后，这么久没有新块就算取空了。
    const QUIET: Duration = Duration::from_millis(400);
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
            match timeout(SCENARIO_LIMIT, record(s, &out)).await {
                Ok(Ok(frames)) => {
                    eprintln!("{}：{frames} 帧", s.name);
                    index.push(s);
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

    /// 目录：网页的预览页与测试按它找录像。手写 JSON，免得为一个小文件加依赖。
    fn index_json(list: &[&Scenario]) -> String {
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
            .map(|s| {
                let focus = s
                    .focus
                    .map_or("null".to_owned(), |f| format!("\"{}\"", esc(f)));
                format!(
                    "  {{\"name\": \"{}\", \"about\": \"{}\", \"focus\": {focus}}}",
                    esc(s.name),
                    esc(s.about)
                )
            })
            .collect();
        format!("[\n{}\n]\n", items.join(",\n"))
    }

    /// 取一块的结果。
    enum Pulled {
        /// 收齐了一帧（编码后的 ViewPatch）。
        Frame(Vec<u8>),
        /// 收到一块，这一帧还没完。
        Partial,
        /// 等了一段时间没有块。
        Quiet,
    }

    /// 网页取帧的最小替身：按块收齐一帧就交出整帧字节。
    struct Puller {
        epoch: u32,
        applied: Option<pb::FramePosition>,
        buf: Vec<u8>,
    }

    impl Puller {
        fn new(epoch: u32) -> Self {
            Self {
                epoch,
                applied: None,
                buf: Vec::new(),
            }
        }

        /// 取一块。
        async fn pull(&mut self, app: &App, wait: Duration) -> Result<Pulled, String> {
            let req = pb::NextFrame {
                epoch: self.epoch,
                applied: self.applied,
            };
            let chunk = match timeout(wait, app.next_frame(req)).await {
                Err(_) => return Ok(Pulled::Quiet),
                Ok(Err(e)) => return Err(format!("取帧失败：{e:?}")),
                Ok(Ok(c)) => c,
            };
            if chunk.index == 0 {
                self.buf.clear();
            }
            self.buf.extend_from_slice(&chunk.data);
            self.applied = Some(pb::FramePosition {
                frame: chunk.frame,
                index: chunk.index,
            });
            Ok(if chunk.last {
                Pulled::Frame(std::mem::take(&mut self.buf))
            } else {
                Pulled::Partial
            })
        }
    }

    /// 假后台里有没有这个对话（侧栏只列最近 5 个，不能拿它判断）。
    fn exists(daemon: &FakeDaemon, conversation: &str) -> bool {
        daemon
            .try_query(&pb::Query {
                kind: Some(pb::query::Kind::GetConversation(pb::GetConversation {
                    conversation_id: conversation.into(),
                    before_message_id: None,
                    limit: 1,
                })),
            })
            .is_ok()
    }

    fn has_rows(patch: &pb::ViewPatch, conversation: &str) -> bool {
        patch.changes.iter().any(|c| {
            matches!(&c.change, Some(pb::view_change::Change::Conversation(p))
                if p.conversation_id == conversation && !p.rows.is_empty())
        })
    }

    fn visible(conversation: Option<&str>) -> pb::UiIntent {
        pb::UiIntent {
            kind: Some(I::SetVisible(pb::SetVisible {
                sidebar: true,
                pending: true,
                settings: true,
                conversation_id: conversation.map(Into::into),
                ..Default::default()
            })),
        }
    }

    async fn declare(app: &App, conversation: Option<&str>) -> Result<(), String> {
        match app.intent(visible(conversation)).await.error {
            None => Ok(()),
            Some(e) => Err(format!("SetVisible 被拒：{} {}", e.code, e.message)),
        }
    }

    async fn record(s: &Scenario, out: &Path) -> Result<usize, String> {
        let dir = tempfile::Builder::new()
            .prefix("t09-fixtures")
            .tempdir_in("/tmp")
            .map_err(|e| format!("临时目录：{e}"))?;
        let daemon = FakeDaemon::start(dir.path().join("d.sock"))
            .await
            .map_err(|e| format!("假后台起不来：{e}"))?;
        let identity = self_trusting_identity().map_err(|e| format!("本进程身份：{e}"))?;
        let (app, _effects) = App::start(
            DirectDialer::new(daemon.endpoint(), identity, "fixtures"),
            Options::default(),
        );
        let epoch = app.attach().await.map_err(|e| format!("attach：{e:?}"))?;
        declare(&app, None).await?;

        let frames = first_epoch(s, &daemon, &app, epoch).await?;

        // 全量：重新 attach（换代次）后的第一帧。
        let epoch = app
            .attach()
            .await
            .map_err(|e| format!("再次 attach：{e:?}"))?;
        let mut puller = Puller::new(epoch);
        let full = loop {
            match puller.pull(&app, Duration::from_secs(5)).await? {
                Pulled::Frame(bytes) => break bytes,
                Pulled::Partial => {}
                Pulled::Quiet => return Err("重新 attach 后等不到全量帧".into()),
            }
        };

        let mut buf = Vec::new();
        for f in &frames {
            prost::encoding::encode_varint(f.len() as u64, &mut buf);
            buf.extend_from_slice(f);
        }
        write(&out.join(format!("{}.frames.binpb", s.name)), &buf)?;
        let mut one = Vec::new();
        prost::encoding::encode_varint(full.len() as u64, &mut one);
        one.extend_from_slice(&full);
        write(&out.join(format!("{}.full.binpb", s.name)), &one)?;
        daemon.shutdown().await;
        Ok(frames.len())
    }

    /// 第一代：场景与取帧同时跑，记下收齐的每一帧。
    async fn first_epoch(
        s: &Scenario,
        daemon: &FakeDaemon,
        app: &App,
        epoch: u32,
    ) -> Result<Vec<Vec<u8>>, String> {
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut puller = Puller::new(epoch);
        let mut focused = s.focus.is_none();
        // focus 对话的行到了没有：声明打开后快照可能还没到，安静了也要等它（CI 上慢）。
        let mut arrived = s.focus.is_none();
        let run = daemon.run_scenario(s.name);
        tokio::pin!(run);
        let mut done = false;
        loop {
            // 场景还在跑：边跑边取；跑完以后取到安静为止。
            let got = if done {
                puller.pull(app, QUIET).await?
            } else {
                tokio::select! {
                    r = &mut run => {
                        r.map_err(|e| e.to_string())?;
                        done = true;
                        continue;
                    }
                    f = puller.pull(app, QUIET) => f?,
                }
            };
            if !focused && s.focus.is_some_and(|c| exists(daemon, c)) {
                declare(app, s.focus).await?;
                focused = true;
            }
            match got {
                Pulled::Frame(bytes) => {
                    let patch = pb::ViewPatch::decode(bytes.as_slice())
                        .map_err(|e| format!("帧解不开：{e}"))?;
                    arrived |= s.focus.is_some_and(|c| has_rows(&patch, c));
                    frames.push(bytes);
                }
                Pulled::Partial => {}
                Pulled::Quiet if done && focused && arrived => break,
                Pulled::Quiet if done && !focused => {
                    return Err(format!(
                        "场景跑完了，假后台里一直没有 focus 对话 {:?}",
                        s.focus
                    ));
                }
                Pulled::Quiet => {}
            }
        }

        Ok(frames)
    }

    fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
        std::fs::write(path, bytes).map_err(|e| format!("写不了 {}：{e}", path.display()))
    }
}
