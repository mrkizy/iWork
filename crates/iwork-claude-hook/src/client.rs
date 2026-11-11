//! hook 一侧：从环境变量取通道参数，连桥接、先验对端、再发请求、读答复（T06 方案 2.4）。
//!
//! 只用 std 与平台调用，同步阻塞。时限不在这里管：整个进程由 [`crate::deadline`] 的看门狗兜住，
//! 连接、写、读卡在哪一步都一样会到时拒绝。这里任何一步不对都返回 [`Failure`]，调用方按拒绝输出。

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use crate::wire::{self, HookKind, RequestHeader, Response};

/// 桥接经 Claude Code 交给 hook 的四个环境变量（T06 方案 2.4；G9a 实测 CLI 会不会传下来）。
pub const ENV_SOCK: &str = "IWORK_HOOK_SOCK";
pub const ENV_TOKEN: &str = "IWORK_HOOK_TOKEN";
pub const ENV_BRIDGE_PID: &str = "IWORK_HOOK_BRIDGE_PID";
pub const ENV_DEADLINE: &str = "IWORK_HOOK_DEADLINE_SECS";

/// hook 拒绝的原因。`reason()` 是回给模型的话，不带路径、令牌之类的细节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// 环境变量缺失或格式不对（方案 4.6 第 1 行）
    ChannelUnset,
    /// socket 不存在、连接被拒（第 2 行）
    Connect,
    /// 对端不是同一用户，或进程号不是桥接（第 3 行）
    PeerMismatch,
    /// 等答复时连接断了（桥接崩溃、被杀，第 7 行）
    Disconnected,
    /// 答复不是合法帧，或取值不认识（第 8、9 行）
    BadResponse,
    /// 工具输入超过上限（第 10 行）
    InputTooLarge,
    /// hook 自己的时限到了，桥接还没答复（第 14 行）
    Deadline,
    /// hook 程序自己出错（panic，第 15 行）
    Internal,
    /// 子命令不认识
    BadArguments,
    /// 这个平台上通道还没接入（Windows 命名管道随桥接在 S6 实现）
    Unsupported,
}

impl Failure {
    pub fn reason(self) -> &'static str {
        match self {
            Self::ChannelUnset => "iWork 审批通道未配置，这次调用没有执行",
            Self::Connect => "连不上 iWork，这次调用没有执行",
            Self::PeerMismatch => "iWork 审批通道对端校验失败，这次调用没有执行",
            Self::Disconnected => "iWork 审批通道中断，这次调用没有执行",
            Self::BadResponse => "iWork 返回了无法识别的结果，这次调用没有执行",
            Self::InputTooLarge => "工具输入超过 4 MiB，iWork 无法审批，请拆小",
            Self::Deadline => "iWork 没能在时限内给出审批结果，这次调用没有执行",
            Self::Internal => "iWork 审批程序出错，这次调用没有执行",
            Self::BadArguments => "iWork 审批程序的参数不对，这次调用没有执行",
            Self::Unsupported => "这个平台上 iWork 审批通道还没接入，这次调用没有执行",
        }
    }
}

/// 通道参数。令牌不进 `Debug`。
#[derive(Clone, PartialEq, Eq)]
pub struct ChannelEnv {
    pub socket: PathBuf,
    token: String,
    pub bridge_pid: u32,
    /// 从 hook 进程启动算起，最晚多久必须给出结论
    pub deadline: Duration,
}

impl std::fmt::Debug for ChannelEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelEnv")
            .field("socket", &self.socket)
            .field("token", &"<redacted>")
            .field("bridge_pid", &self.bridge_pid)
            .field("deadline", &self.deadline)
            .finish()
    }
}

impl ChannelEnv {
    /// 从本进程的环境变量取。
    pub fn from_env() -> Result<Self, Failure> {
        Self::from_vars(|name| std::env::var_os(name))
    }

    /// 从给定的查找函数取：四个都要在，格式都要对；令牌是 64 位十六进制，进程号与时限是正整数。
    pub fn from_vars(get: impl Fn(&str) -> Option<OsString>) -> Result<Self, Failure> {
        let text = |name: &str| get(name).and_then(|v| v.into_string().ok());
        let positive = |name: &str| {
            text(name)
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|n| *n > 0)
        };
        let socket = get(ENV_SOCK)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .ok_or(Failure::ChannelUnset)?;
        let token = text(ENV_TOKEN)
            .filter(|t| wire::parse_token(t).is_some())
            .ok_or(Failure::ChannelUnset)?;
        let bridge_pid = positive(ENV_BRIDGE_PID)
            .and_then(|p| u32::try_from(p).ok())
            .ok_or(Failure::ChannelUnset)?;
        let deadline = positive(ENV_DEADLINE)
            .map(Duration::from_secs)
            .ok_or(Failure::ChannelUnset)?;
        Ok(Self {
            socket,
            token,
            bridge_pid,
            deadline,
        })
    }
}

/// 问桥接一次。只有拿到一条格式正确、来自已校验对端的答复才返回 `Ok`；`Ok` 里的决定可能是拒绝。
pub fn ask(env: &ChannelEnv, kind: HookKind, stdin: &[u8]) -> Result<Response, Failure> {
    if stdin.len() > wire::MAX_BODY_BYTES as usize {
        return Err(Failure::InputTooLarge);
    }
    let header = RequestHeader {
        kind,
        token: env.token.clone(),
        pid: std::process::id(),
    };
    imp::exchange(env, &header, stdin)
}

#[cfg(unix)]
mod imp {
    use std::os::unix::net::UnixStream;

    use super::{ChannelEnv, Failure};
    use crate::peer;
    use crate::wire::{self, RequestHeader, Response, WireError};

    pub fn exchange(
        env: &ChannelEnv,
        header: &RequestHeader,
        body: &[u8],
    ) -> Result<Response, Failure> {
        let mut conn = UnixStream::connect(&env.socket).map_err(|_| Failure::Connect)?;
        // 先验对端再发任何字节：令牌不交给冒充桥接的进程
        let cred = peer::peer_cred(&conn).map_err(|_| Failure::PeerMismatch)?;
        if cred.user != peer::current_user() || cred.pid != env.bridge_pid {
            return Err(Failure::PeerMismatch);
        }
        wire::write_request(&mut conn, header, body).map_err(|e| match e {
            WireError::TooLarge { .. } => Failure::InputTooLarge,
            _ => Failure::Disconnected,
        })?;
        wire::read_response(&mut conn).map_err(|e| match e {
            WireError::Io(_) | WireError::Eof => Failure::Disconnected,
            WireError::BadMagic | WireError::TooLarge { .. } | WireError::Malformed(_) => {
                Failure::BadResponse
            }
        })
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{ChannelEnv, Failure};
    use crate::wire::{RequestHeader, Response};

    pub fn exchange(
        _env: &ChannelEnv,
        _header: &RequestHeader,
        _body: &[u8],
    ) -> Result<Response, Failure> {
        Err(Failure::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn good() -> Vec<(&'static str, String)> {
        vec![
            (ENV_SOCK, "/run/h/ab/s".into()),
            (ENV_TOKEN, "0f".repeat(32)),
            (ENV_BRIDGE_PID, "4242".into()),
            (ENV_DEADLINE, "7185".into()),
        ]
    }

    fn from(pairs: &[(&'static str, String)]) -> Result<ChannelEnv, Failure> {
        let borrowed: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        ChannelEnv::from_vars(vars(&borrowed))
    }

    #[test]
    fn complete_env_parses() {
        let env = from(&good()).unwrap();
        assert_eq!(env.socket, PathBuf::from("/run/h/ab/s"));
        assert_eq!(env.bridge_pid, 4242);
        assert_eq!(env.deadline, Duration::from_secs(7185));
        assert!(
            !format!("{env:?}").contains(&"0f".repeat(32)),
            "令牌不进 Debug"
        );
    }

    /// 方案 4.6 第 1 行：任何一个缺了或格式不对，都是「通道未配置」。
    #[test]
    fn any_missing_or_malformed_var_is_unset() {
        for name in [ENV_SOCK, ENV_TOKEN, ENV_BRIDGE_PID, ENV_DEADLINE] {
            let missing: Vec<_> = good().into_iter().filter(|(k, _)| *k != name).collect();
            assert_eq!(from(&missing), Err(Failure::ChannelUnset), "缺 {name}");
        }
        for (name, bad) in [
            (ENV_SOCK, ""),
            (ENV_TOKEN, "short"),
            (ENV_TOKEN, &"zz".repeat(32)),
            (ENV_BRIDGE_PID, "0"),
            (ENV_BRIDGE_PID, "-1"),
            (ENV_BRIDGE_PID, "99999999999"),
            (ENV_DEADLINE, "0"),
            (ENV_DEADLINE, "soon"),
        ] {
            let pairs: Vec<_> = good()
                .into_iter()
                .map(|(k, v)| {
                    if k == name {
                        (k, bad.to_string())
                    } else {
                        (k, v)
                    }
                })
                .collect();
            assert_eq!(from(&pairs), Err(Failure::ChannelUnset), "{name}={bad}");
        }
    }

    #[test]
    fn oversize_input_is_refused_without_connecting() {
        let env = from(&good()).unwrap();
        let big = vec![b'x'; wire::MAX_BODY_BYTES as usize + 1];
        // socket 路径不存在：要是先去连接，会得到 Connect 而不是 InputTooLarge
        assert_eq!(
            ask(&env, HookKind::PreToolUse, &big),
            Err(Failure::InputTooLarge)
        );
    }
}
