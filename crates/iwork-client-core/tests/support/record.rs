//! 录一个场景（scenario_fold 测试与 dump-view-fixtures 录像工具共用；示例程序用 `#[path]` 引这个文件）。
//!
//! 两段式（审查 2a 1/F-2）：客户端先连上（Live）；跑到建出 focus 对话为止，声明打开它、等快照到；
//! 再跑剩下的事件，这一段的帧才是对话的增量。没有 focus 的场景连上以后整段跑，增量落在侧栏上。
//! 收敛条件：场景跑完、连上（Live）、安静了 [`QUIET`]；之后重新 attach 拿一帧全量。
//! 场景以「停止监听」收尾（断线状态）时，最后一段不要求连上。
//! 场景的 `client` 动作（往上翻、停在旧消息、拖入超大文件）在打开对话之后、跑剩下的事件之前做。

#![allow(dead_code, reason = "测试与示例各用其中一部分")]

use std::time::Duration;

use iwork_client_core::{App, DirectDialer, Options, ShellEvent};
use iwork_fake_daemon::scenario::{ClientAct, Scenario, Step};
use iwork_fake_daemon::{FakeDaemon, self_trusting_identity};
use iwork_proto::pb;
use prost::Message;
use tokio::time::timeout;

/// 这么久没有新块就算安静。要盖住一轮摘要重查（节流 250ms 加查询往返），CI 上 400ms 不够（run 42992011638）。
pub const QUIET: Duration = Duration::from_millis(1200);

pub struct Recording {
    /// 第一代收齐的每一帧（编码后的 ViewPatch）。
    pub frames: Vec<Vec<u8>>,
    /// 重新 attach 后的全量帧。
    pub full: Vec<u8>,
    /// 打开对话（没有 focus 时是连上）时已经收到几帧：`frames[opened..]` 是之后的帧。
    pub opened: usize,
    /// 打开对话（没有 focus 时是连上）之后，带着增量的帧数：focus 对话的行或流式追加、或侧栏。
    pub increments: usize,
}

enum Pulled {
    Frame(Vec<u8>),
    Partial,
    Quiet,
}

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

/// `home`：这份录像停在首页（场景没有 focus）。有 focus 的录像连上时也不声明首页，免得折叠结果里留着一份
/// 打开对话后就不再更新的首页视图。
fn visible(conversation: Option<&str>, home: bool) -> pb::UiIntent {
    pb::UiIntent {
        kind: Some(pb::ui_intent::Kind::SetVisible(pb::SetVisible {
            sidebar: true,
            pending: true,
            settings: true,
            conversation_id: conversation.map(Into::into),
            // 没有 focus 的录像停在首页（预览页与组件测试的首页靠它）
            home: home.then(|| pb::HomeQuery {
                project_id: String::new(),
            }),
            ..Default::default()
        })),
    }
}

async fn declare(app: &App, conversation: Option<&str>, home: bool) -> Result<(), String> {
    match app.intent(visible(conversation, home)).await.error {
        None => Ok(()),
        Some(e) => Err(format!("SetVisible 被拒：{} {}", e.code, e.message)),
    }
}

/// 打开之后带着增量的帧数：有 focus 时是对话的行或流式追加、或输入框一带变了（和上一次的比，
/// 不是只要带着就算——每帧都整份带输入框）；没有 focus 时是侧栏。
fn count_increments(frames: &[Vec<u8>], opened: usize, focus: Option<&str>) -> usize {
    let mut composer: Option<pb::ComposerView> = None;
    let mut n = 0;
    for (i, f) in frames.iter().enumerate() {
        let p = pb::ViewPatch::decode(f.as_slice()).expect("前面解过");
        let seen = inspect(&p, focus);
        let mut composer_changed = false;
        for c in &p.changes {
            if let Some(pb::view_change::Change::Conversation(cp)) = &c.change
                && Some(cp.conversation_id.as_str()) == focus
                && cp.composer.is_some()
                && cp.composer != composer
            {
                composer_changed = composer.is_some();
                composer.clone_from(&cp.composer);
            }
        }
        let counts = if focus.is_some() {
            seen.conversation_rows || composer_changed
        } else {
            seen.sidebar
        };
        if i >= opened && counts {
            n += 1;
        }
    }
    n
}

/// 以「停止监听」收尾、之后没再恢复：客户端停在断线状态。
fn ends_offline(steps: &[Step]) -> bool {
    steps
        .iter()
        .rev()
        .find_map(|s| match s {
            Step::StopListening => Some(true),
            Step::DisconnectAll | Step::DisconnectForSnapshot => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}

/// 做一个客户端动作，等它的帧收齐、安静下来。
async fn client_act(
    pump: &mut Pump<'_>,
    daemon: &FakeDaemon,
    app: &App,
    act: ClientAct,
    focus: &str,
    dir: &std::path::Path,
) -> Result<(), String> {
    match act {
        ClientAct::LoadOlder => {
            let ack = app
                .intent(pb::UiIntent {
                    kind: Some(pb::ui_intent::Kind::LoadOlder(pb::LoadOlder {
                        conversation_id: focus.into(),
                    })),
                })
                .await;
            if let Some(e) = ack.error {
                return Err(format!("LoadOlder 被拒：{}", e.message));
            }
        }
        ClientAct::ReadOldest => {
            let oldest = oldest_ordinal(&pump.frames, focus)
                .ok_or_else(|| format!("对话 {focus} 还没有行，停不到旧消息上"))?;
            let ack = app
                .intent(pb::UiIntent {
                    kind: Some(pb::ui_intent::Kind::SetVisibleRows(pb::SetVisibleRows {
                        conversation_id: focus.into(),
                        first_ordinal: oldest,
                        last_ordinal: oldest + 3,
                        by_user: false,
                    })),
                })
                .await;
            if let Some(e) = ack.error {
                return Err(format!("SetVisibleRows 被拒：{}", e.message));
            }
        }
        ClientAct::DragHover => app.shell(ShellEvent::Dragging(true)),
        ClientAct::DropOversized => {
            // 稀疏文件：只占目录项，不真写 21MB
            let path = dir.join("超过二十兆.bin");
            let f = std::fs::File::create(&path).map_err(|e| format!("建大文件：{e}"))?;
            f.set_len(21 * 1024 * 1024)
                .map_err(|e| format!("设大文件长度：{e}"))?;
            app.shell(ShellEvent::Dropped(vec![
                path.to_string_lossy().into_owned(),
            ]));
        }
    }
    pump.run(daemon, Vec::new(), |_| true).await
}

/// 录到的帧里这个对话最老一行的 ordinal。
fn oldest_ordinal(frames: &[Vec<u8>], focus: &str) -> Option<i64> {
    frames
        .iter()
        .filter_map(|f| pb::ViewPatch::decode(f.as_slice()).ok())
        .flat_map(|p| p.changes)
        .filter_map(|c| match c.change {
            Some(pb::view_change::Change::Conversation(p)) if p.conversation_id == focus => Some(p),
            _ => None,
        })
        .flat_map(|p| p.rows)
        .map(|r| r.ordinal)
        .min()
}

/// 一帧里看得到的几件事。
#[derive(Default)]
struct Seen {
    live: Option<bool>,
    conversation_opened: bool,
    conversation_rows: bool,
    sidebar: bool,
}

fn inspect(patch: &pb::ViewPatch, focus: Option<&str>) -> Seen {
    let mut s = Seen::default();
    for c in &patch.changes {
        match &c.change {
            Some(pb::view_change::Change::Connection(p)) => {
                if let Some(v) = &p.view {
                    s.live = Some(v.state() == pb::ConnectionState::Live);
                }
            }
            Some(pb::view_change::Change::Conversation(p))
                if Some(p.conversation_id.as_str()) == focus =>
            {
                s.conversation_opened = true;
                s.conversation_rows |= !p.rows.is_empty() || !p.appends.is_empty();
            }
            Some(pb::view_change::Change::Sidebar(p)) => s.sidebar |= p.view.is_some(),
            _ => {}
        }
    }
    s
}

/// 一边跑一段步骤一边取帧，直到这段跑完、连上、而且安静；`until` 另外要求的条件也要满足。
struct Pump<'a> {
    app: &'a App,
    puller: Puller,
    frames: Vec<Vec<u8>>,
    live: bool,
    focus: Option<&'a str>,
    /// 这一段收尾时要不要求连上（以「停止监听」收尾的场景不要求）。
    need_live: bool,
}

impl Pump<'_> {
    async fn take(&mut self, wait: Duration) -> Result<Option<Seen>, String> {
        loop {
            match self.puller.pull(self.app, wait).await? {
                Pulled::Frame(bytes) => {
                    let patch = pb::ViewPatch::decode(bytes.as_slice())
                        .map_err(|e| format!("帧解不开：{e}"))?;
                    let seen = inspect(&patch, self.focus);
                    if let Some(l) = seen.live {
                        self.live = l;
                    }
                    self.frames.push(bytes);
                    return Ok(Some(seen));
                }
                Pulled::Partial => {}
                Pulled::Quiet => return Ok(None),
            }
        }
    }

    /// 跑完 `steps`（同时取帧），再取到「连上且安静、而且 `until` 成立过」。
    async fn run(
        &mut self,
        daemon: &FakeDaemon,
        steps: Vec<iwork_fake_daemon::scenario::Step>,
        mut until: impl FnMut(&Seen) -> bool,
    ) -> Result<(), String> {
        let run = daemon.run_steps(steps);
        tokio::pin!(run);
        let mut done = false;
        let mut met = false;
        loop {
            let got = if done {
                self.take(QUIET).await?
            } else {
                tokio::select! {
                    () = &mut run => { done = true; continue; }
                    g = self.take(QUIET) => g?,
                }
            };
            match got {
                Some(seen) => met |= until(&seen),
                None if done && (self.live || !self.need_live) && met => return Ok(()),
                None => {}
            }
        }
    }
}

/// 场景登记的形象映射（只给录像与预览）放进 `opts.characters`。
pub async fn record(s: &Scenario, mut opts: Options) -> Result<Recording, String> {
    if let Some(cap) = s.timeline_cap {
        opts.timeline_cap = cap;
    }
    opts.characters = s
        .characters
        .iter()
        .map(|(a, c)| ((*a).to_owned(), (*c).to_owned()))
        .collect();
    let dir = tempfile::Builder::new()
        .prefix("t09-record")
        .tempdir_in("/tmp")
        .map_err(|e| format!("临时目录：{e}"))?;
    let daemon = FakeDaemon::start(dir.path().join("d.sock"))
        .await
        .map_err(|e| format!("假后台起不来：{e}"))?;
    let identity = self_trusting_identity().map_err(|e| format!("本进程身份：{e}"))?;
    let (app, _effects) = App::start(
        DirectDialer::new(daemon.endpoint(), identity, "record"),
        opts,
    );
    let epoch = app.attach().await.map_err(|e| format!("attach：{e:?}"))?;
    declare(&app, None, s.focus.is_none()).await?;
    let mut pump = Pump {
        app: &app,
        puller: Puller::new(epoch),
        frames: Vec::new(),
        live: false,
        focus: s.focus,
        need_live: true,
    };
    // 先连上
    pump.run(&daemon, Vec::new(), |_| true).await?;
    let (before, after) = s.split_at_focus();
    // 建出 focus 对话，打开它，等它的快照到
    if let Some(focus) = s.focus {
        pump.run(&daemon, before, |_| true).await?;
        declare(&app, Some(focus), false).await?;
        pump.run(&daemon, Vec::new(), |seen| seen.conversation_opened)
            .await?;
    }
    let opened = pump.frames.len();
    for act in s.client {
        let focus = s
            .focus
            .ok_or_else(|| format!("场景 {} 有客户端动作却没有 focus", s.name))?;
        client_act(&mut pump, &daemon, &app, *act, focus, dir.path()).await?;
    }
    // 剩下的事件：这一段的帧才是增量
    pump.need_live = !ends_offline(&after);
    pump.run(&daemon, after, |_| true).await?;
    let increments = count_increments(&pump.frames, opened, s.focus);
    let frames = pump.frames;

    // 全量：重新 attach（换代次）后的第一帧
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
    daemon.shutdown().await;
    Ok(Recording {
        frames,
        full,
        opened,
        increments,
    })
}
