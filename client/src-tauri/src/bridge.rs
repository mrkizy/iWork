//! 三个命令（T09 方案 2.4 第 7 条）：`attach`、`next_frame`、`ui_intent`，全走原始字节，不用 `Channel`。
//!
//! 入口先认传输路径：只有自定义协议（`ipc://`）那条路的请求头里有 `Tauri-Callback`（头名不区分大小写）。
//! 没有就是回退到了 postMessage：不算帧、不交任何数据、不执行操作，只回错误 `transport_fallback`。
//! 这个头只用来区分路径，不是认证凭证（来源与窗口由 capability 管，E6）。
//! 判断与分发写成不依赖 Tauri 运行时的函数，单测直接调。

use std::sync::atomic::{AtomicU64, Ordering};

use iwork_client_core::{App, FrameError};
use iwork_proto::pb;
use prost::Message;
use tauri::http::HeaderMap;

/// 回退路径的错误码；网页收到它就停掉取帧、显示固定诊断页。
pub const TRANSPORT_FALLBACK: &str = "transport_fallback";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Attach,
    NextFrame,
    UiIntent,
}

/// 按路径分开的调用计数（真实 WebView 测试断言用）。
#[derive(Debug, Default)]
pub struct PathCounters {
    pub custom_protocol: AtomicU64,
    pub fallback: AtomicU64,
}

/// 请求走的是不是自定义协议那条路。
pub fn via_custom_protocol(headers: &HeaderMap) -> bool {
    headers
        .keys()
        .any(|k| k.as_str().eq_ignore_ascii_case("tauri-callback"))
}

/// 一次命令调用：先认路径，回退路径一律不碰会话。
pub async fn dispatch(
    app: &App,
    counters: &PathCounters,
    cmd: Cmd,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Vec<u8>, String> {
    if !via_custom_protocol(headers) {
        counters.fallback.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(target: "iwork_client", ?cmd, "invoke 回退到了 postMessage，不交数据");
        return Err(TRANSPORT_FALLBACK.to_owned());
    }
    counters.custom_protocol.fetch_add(1, Ordering::Relaxed);
    match cmd {
        Cmd::Attach => {
            let epoch = app.attach().await.map_err(frame_error)?;
            Ok(pb::AttachReply { epoch }.encode_to_vec())
        }
        Cmd::NextFrame => {
            let req = pb::NextFrame::decode(body).map_err(|e| format!("bad_request: {e}"))?;
            let chunk = app.next_frame(req).await.map_err(frame_error)?;
            Ok(chunk.encode_to_vec())
        }
        Cmd::UiIntent => {
            let intent = pb::UiIntent::decode(body).map_err(|e| format!("bad_request: {e}"))?;
            Ok(app.intent(intent).await.encode_to_vec())
        }
    }
}

fn frame_error(e: FrameError) -> String {
    match e {
        FrameError::StaleEpoch => "stale_epoch",
        FrameError::Superseded => "superseded",
        FrameError::Closed => "closed",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwork_client_core::{DialError, Dialer, Options};
    use iwork_ipc::client::{Client, Incoming};
    use tauri::http::HeaderValue;

    /// 永远连不上的拨号器：这里只测入口，不需要后台。
    struct Never;
    impl Dialer for Never {
        async fn dial(&mut self) -> Result<(Client, Incoming), DialError> {
            std::future::pending().await
        }
    }

    fn headers(name: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_static("123"));
        h
    }

    fn body(cmd: Cmd) -> Vec<u8> {
        match cmd {
            Cmd::Attach => Vec::new(),
            Cmd::NextFrame => pb::NextFrame {
                epoch: 1,
                applied: None,
            }
            .encode_to_vec(),
            Cmd::UiIntent => pb::UiIntent {
                kind: Some(pb::ui_intent::Kind::Quit(pb::QuitIntent { confirm: false })),
            }
            .encode_to_vec(),
        }
    }

    /// 回退路径（请求头里没有 Tauri-Callback）：三个命令都只回 transport_fallback，帧引擎不交字节、
    /// 不算帧、操作不执行；有这个头时照常，头名大小写写法不同也认得出。
    #[tokio::test]
    async fn fallback_path_gets_no_frame_data() {
        let (app, _effects) = App::start(Never, Options::default());
        let counters = PathCounters::default();
        let stats = app.stats();
        for cmd in [Cmd::Attach, Cmd::NextFrame, Cmd::UiIntent] {
            for h in [
                HeaderMap::new(),
                headers("tauri-error"),
                headers("tauri-invoke-key"),
            ] {
                let r = dispatch(&app, &counters, cmd, &h, &body(cmd)).await;
                assert_eq!(r, Err(TRANSPORT_FALLBACK.to_owned()), "{cmd:?}");
            }
        }
        assert_eq!(counters.fallback.load(Ordering::Relaxed), 9);
        assert_eq!(counters.custom_protocol.load(Ordering::Relaxed), 0);
        let s = stats.borrow().clone();
        assert_eq!(
            (s.frames_built, s.handed_bytes),
            (0, 0),
            "回退路径上帧引擎什么都没交"
        );

        // 对照：自定义协议路径照常（HeaderMap 的头名总是小写；大小写写法不同的输入在插入时就归一了，
        // 这里再直接比一遍原始写法）。
        assert!(via_custom_protocol(&headers("tauri-callback")));
        let mut mixed = HeaderMap::new();
        mixed.insert(
            tauri::http::HeaderName::from_bytes(b"Tauri-Callback").unwrap(),
            HeaderValue::from_static("1"),
        );
        assert!(via_custom_protocol(&mixed));
        let epoch_bytes = dispatch(&app, &counters, Cmd::Attach, &mixed, &[])
            .await
            .unwrap();
        let epoch = pb::AttachReply::decode(epoch_bytes.as_slice())
            .unwrap()
            .epoch;
        assert!(epoch > 0);
        let chunk = dispatch(
            &app,
            &counters,
            Cmd::NextFrame,
            &mixed,
            &pb::NextFrame {
                epoch,
                applied: None,
            }
            .encode_to_vec(),
        )
        .await
        .unwrap();
        assert!(
            !pb::FrameChunk::decode(chunk.as_slice())
                .unwrap()
                .data
                .is_empty(),
            "对照：有头时交出全量首帧"
        );
        assert_eq!(counters.custom_protocol.load(Ordering::Relaxed), 2);
    }
}
