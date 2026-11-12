//! 登录自启（T04 方案 2.12）：让系统的服务管理器在用户登录时起后台服务，并在它异常退出时再拉。
//! 实现在 `iwork-host::autostart`（macOS 是 LaunchAgent plist + `launchctl`，Linux 是 systemd 用户单元）。
//!
//! 设置是意图、注册项是实际状态：受理 `SetAutostart` 只提交事件，从不调这个端口的 `set`；全进程只有后台服务的
//! 对齐任务调 `set`，让系统跟上已提交的设置。

use std::future::Future;
use std::path::PathBuf;

/// 系统里现在的注册状态。注册项由两半组成：定义文件（plist / 单元文件）与服务管理器里的状态（已加载 / 已启用）；
/// 两半都以实际查到的为准，不看上次 `set` 返回了什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutostartState {
    /// 两半都在：定义文件在、服务管理器认它。`program` 是定义文件里写的后台程序路径。
    Registered { program: PathBuf },
    /// 两半都不在。
    NotRegistered,
    /// 只剩一半：注册到一半失败（文件写了、加载或启用没成），或注销到一半被打断（文件删了、服务还在）。
    /// 对齐任务按意图把它补齐或清掉。
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AutostartError {
    /// 这个平台或环境做不了自启（如 Linux 没有 systemd 用户实例；M1 的 Windows）。
    #[error("这台机器上做不了登录自启：{reason}")]
    Unsupported { reason: String },
    #[error("登录自启的注册或注销失败：{message}")]
    Failed { message: String },
}

impl AutostartError {
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

pub trait AutostartPort: Send + Sync {
    fn state(&self) -> impl Future<Output = Result<AutostartState, AutostartError>> + Send;
    /// 这个平台能不能做自启。只读，不改任何东西。
    fn supported(&self) -> Result<(), AutostartError>;
    /// 幂等：已是目标状态时什么都不做；从 [`AutostartState::Partial`] 出发也能走到目标状态。
    /// `set(true)` 会让服务管理器**立刻**起一个托管实例（它撞锁后等锁接班）。
    /// `set(false)` 会**结束**服务管理器名下正在跑的那个实例——可能就是调用者自己，所以调用可能不返回。
    /// 实现必须先删定义文件、最后才结束服务：调用者在任何一步被结束，留下的都只是服务管理器里的状态
    /// （下次登录不会再起），下次对齐查到 `Partial` 再清。
    fn set(&self, enabled: bool) -> impl Future<Output = Result<(), AutostartError>> + Send;
}
