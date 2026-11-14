//! 关窗后回合继续（T10a 任务卡验收 2；方案 7.1）：真存储 + 真引擎 + 真回合宿主 + 假运行时，客户端经 ipc
//! （服务端、帧、连接生命周期都是真的，传输用进程内的内存管道）发消息；回合进行中客户端断开，宿主照样把回合跑完、
//! 结果落盘；重连订阅对话拿快照，和一直连着的那一次逐项相同。

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use iwork_daemon::attention::Attention;
use iwork_daemon::handler::query::{Queries, QueryOr};
use iwork_daemon::snapshot::ReadSnapshots;
use iwork_daemon::source::StoreSource;
use iwork_domain::event::DomainEvent;
use iwork_domain::id::TurnId;
use iwork_domain::turn::TurnState;
use iwork_ipc::client::{Client, ClientConfig, Incoming, IncomingItem};
use iwork_ipc::peer::{ImageId, Manifest, PeerIdentity, SelfIdentity, SignerIdentity, UserId};
use iwork_ipc::server::{ChunkRefused, ConnectionContext, RequestHandler, Server, ServerConfig};
use iwork_ipc::transport::Endpoint;
use iwork_ipc::transport::memory::{self, MemoryConnector};
use iwork_proto::pb;
use support::fixtures::*;
use support::turns::*;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// 分发入口的共享句柄，外加「连接断开」的计数（等服务端真的处理完断开再往下走）。
struct Front {
    d: Arc<Disp>,
    closed: Arc<(AtomicUsize, Notify)>,
}

impl RequestHandler for Front {
    async fn handle(&self, ctx: &ConnectionContext, req: pb::Request) -> pb::Response {
        self.d.handle(ctx, req).await
    }

    async fn chunk(&self, ctx: &ConnectionContext, chunk: pb::Chunk) -> Result<(), ChunkRefused> {
        self.d.chunk(ctx, chunk).await
    }

    async fn connection_closed(&self, ctx: &ConnectionContext) {
        self.d.connection_closed(ctx).await;
        self.closed.0.fetch_add(1, Ordering::SeqCst);
        self.closed.1.notify_waiters();
    }
}

fn identity(img: u8) -> PeerIdentity {
    PeerIdentity {
        pid: 1,
        user: UserId::Uid(1),
        exe: Some(PathBuf::from("/i/x")),
        signer: SignerIdentity::Image(ImageId::Sha256([img; 32])),
    }
}

/// 服务端（1）与客户端（2）的身份：同一用户、清单里两个映像都有。
fn own(img: u8) -> SelfIdentity {
    let entries = format!(
        "\"{}\",\"{}\"",
        ImageId::Sha256([1; 32]),
        ImageId::Sha256([2; 32])
    );
    SelfIdentity {
        user: UserId::Uid(1),
        signer: SignerIdentity::Image(ImageId::Sha256([img; 32])),
        exe_dir: PathBuf::from("/i"),
        manifest: Some(
            Manifest::parse(&format!(
                r#"{{"manifest_version":1,"app_version":"0","binaries":{{"x":[{entries}]}}}}"#
            ))
            .unwrap(),
        ),
    }
}

/// 一套：回合宿主跑着，ipc 服务端接在分发入口与查询上，提交观察者把事件发给它。
struct Rig {
    h: H,
    connector: MemoryConnector,
    closed: Arc<(AtomicUsize, Notify)>,
    _stop: tokio_util::sync::DropGuard,
}

impl Rig {
    async fn new() -> Rig {
        let mut h = harness(Opts::default()).await;
        h.agent.hold.store(true, Ordering::SeqCst);
        h.start_host();
        let read = Arc::new(read_model(&h.raw));
        let closed: Arc<(AtomicUsize, Notify)> = Arc::default();
        let front = QueryOr::new(
            Queries::new(read.clone(), Arc::new(Attention::new("boot".into()))),
            Front {
                d: h.d.clone(),
                closed: closed.clone(),
            },
        );
        let (server, publisher) = Server::new(
            ServerConfig::new("test"),
            own(1),
            front,
            StoreSource::new(h.raw.clone(), ReadSnapshots::new(read)),
        );
        h.sink.connect(publisher);
        let (listener, connector) =
            memory::endpoint("disconnect", identity(2), Arc::new(|_| identity(1)));
        let stop = CancellationToken::new();
        tokio::spawn(server.serve(listener, stop.clone()));
        Rig {
            h,
            connector,
            closed,
            _stop: stop.drop_guard(),
        }
    }

    async fn connect(&self) -> (Client, Incoming) {
        Client::connect(
            &self.connector,
            &Endpoint::Memory("disconnect".into()),
            &own(2),
            ClientConfig::new(pb::ClientKind::Desktop, "test"),
        )
        .await
        .unwrap()
    }

    /// 经 ipc 发一条消息，返回这一笔排上的回合。
    async fn send(&self, client: &Client, text: &str) -> TurnId {
        let r = client
            .request(pb::request::Body::Command(pb::Command {
                command_id: fresh_id(1).to_string(),
                kind: Some(pb::command::Kind::SendMessage(pb::SendMessage {
                    conversation_id: conversation().to_string(),
                    text: text.into(),
                    attachment_ids: vec![],
                })),
            }))
            .await
            .unwrap();
        let a = accepted(&r);
        events(&self.h.raw)
            .await
            .into_iter()
            .filter(|(s, _)| *s >= a.first_seq && *s <= a.last_seq)
            .find_map(|(_, e)| match e {
                DomainEvent::TurnQueued(q) => Some(q.turn.id),
                _ => None,
            })
            .expect("这一笔里有 TurnQueued")
    }

    /// 断开：丢掉客户端与它的入口，等服务端处理完这条连接的断开。
    async fn disconnect(&self, client: Client, incoming: Incoming) {
        let before = self.closed.0.load(Ordering::SeqCst);
        drop(client);
        drop(incoming);
        within("服务端处理完断开", async {
            loop {
                let woke = self.closed.1.notified();
                if self.closed.0.load(Ordering::SeqCst) > before {
                    return;
                }
                woke.await;
            }
        })
        .await;
    }
}

/// 订阅对话、取快照里的那一页。
async fn snapshot_page(client: &Client, incoming: &mut Incoming) -> pb::GetConversationResult {
    client
        .subscribe(
            7,
            pb::SubscriptionScope {
                kind: Some(pb::subscription_scope::Kind::Conversation(
                    pb::ConversationScope {
                        conversation_id: conversation().to_string(),
                    },
                )),
            },
            0,
        )
        .await
        .unwrap();
    let snapshot = within("订阅回快照", async {
        loop {
            match incoming.next().await {
                Some(IncomingItem::Subscribed { subscribed, .. }) => {
                    return subscribed.snapshot.expect("from_seq = 0 带快照");
                }
                Some(IncomingItem::Closed(reason)) => panic!("连接被关：{reason:?}"),
                Some(_) => {}
                None => panic!("入口结束了"),
            }
        }
    })
    .await;
    snapshot
        .results
        .into_iter()
        .find_map(|r| match r.kind {
            Some(pb::query_result::Kind::GetConversation(g)) => Some(g),
            _ => None,
        })
        .expect("会话快照里有一页")
}

/// 一页里和编号、时间、seq 无关的内容：消息（作者、状态、正文）、回合（状态与结果）、累计用量。
fn content(g: &pb::GetConversationResult) -> String {
    let messages: Vec<_> = g
        .messages
        .iter()
        .map(|m| format!("{:?} {:?} {:?}", m.author, m.status(), m.body))
        .collect();
    let turns: Vec<_> = g
        .turns
        .iter()
        .map(|t| {
            format!(
                "{:?} {:?} {:?} {:?} {:?} {} {:?}",
                t.state(),
                t.kind(),
                t.interrupt_reason,
                t.state_reason,
                t.usage,
                t.celebrate,
                t.change_summary
            )
        })
        .collect();
    format!(
        "messages={messages:#?}\nturns={turns:#?}\nusage={:?}",
        g.usage_total
    )
}

/// 发消息 → （断开）→ 假运行时跑完 → 订阅取快照。`disconnect` 为假时一直用同一条连接，是对照。
async fn run(disconnect: bool) -> pb::GetConversationResult {
    let rig = Rig::new().await;
    let (client, incoming) = rig.connect().await;
    let t = rig.send(&client, "改一下登录页").await;
    rig.h.agent.wait_prompts(1).await;
    let mut kept = if disconnect {
        rig.disconnect(client, incoming).await;
        // 断开处理完以后宿主至少完整对过一轮账，这时还没发过取消、回合还在跑。取消是在对账的那一轮里同步发的，
        // 所以这里判得准；之后才放行假运行时。叫醒两次、各等一轮：第一轮可能是断开之前就开始的那一轮，第二轮一定
        // 开始在断开处理完之后（宿主空闲时一次叫醒只跑一轮，不能一次等两轮）
        let counters = rig.h.host.counters();
        for _ in 0..2 {
            let rounds = counters.rounds.load(Ordering::SeqCst);
            rig.h.host.wake();
            rig.h.wait_rounds(rounds + 1).await;
        }
        assert_eq!(
            counters.cancels_sent.load(Ordering::SeqCst),
            0,
            "断开不该让宿主取消回合"
        );
        assert_eq!(
            rig.h.turn_state(t).await,
            TurnState::Running,
            "断开以后回合还在跑"
        );
        None
    } else {
        Some((client, incoming))
    };
    rig.h.agent.finish_one();
    let end = rig.h.wait_state(t, |s| s.is_terminal()).await;
    assert_eq!(end, TurnState::Completed, "回合照样跑完");
    let (client, mut incoming) = match kept.take() {
        Some(pair) => pair,
        None => rig.connect().await,
    };
    let page = snapshot_page(&client, &mut incoming).await;
    assert!(
        page.turns
            .iter()
            .any(|x| x.id == t.to_string() && x.state() == pb::TurnState::Completed),
        "快照里这个回合是完成态"
    );
    page
}

/// 回合进行中客户端断开：宿主照样跑完、结果落盘；重连拿到的快照和一直连着的那一次内容相同。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_completes_after_client_disconnect() {
    let control = run(false).await;
    let after = run(true).await;
    assert!(
        control.messages.iter().any(|m| matches!(
            m.author.as_ref().and_then(|a| a.kind.as_ref()),
            Some(pb::author::Kind::Assistant(_))
        )),
        "对照：助手回了话"
    );
    assert_eq!(content(&after), content(&control));
}
