//! Linux：systemd 用户单元（T04 方案 2.12；N4 实测）。
//!
//! - `$XDG_CONFIG_HOME/systemd/user/iwork-daemon.service`：`ExecStart=<绝对路径> --service`、`Restart=on-failure`、
//!   `RestartSec=10`、`StartLimitIntervalSec=300`、`StartLimitBurst=10`、`WantedBy=default.target`。
//! - **不加 `RestartForceExitStatus=SIGTERM`**：被要求停下（SIGTERM 等）不自动再拉，systemd 的默认正合这个意思。
//!   和 launchd 只在后台装上信号处理之前那一小段不一样（方案 2.12 如实写了）。
//! - 开：写单元、`daemon-reload`、`enable --now`；关：`disable --now`、删文件、`daemon-reload`。
//! - 没有 systemd 用户实例 → `Unsupported`。

use std::path::{Path, PathBuf};

use iwork_ports::autostart::{AutostartError, AutostartPort, AutostartState};

use super::{AutostartConfig, SERVICE_ARG, failed, remove, run, write_atomically};

pub const UNIT: &str = "iwork-daemon.service";

pub struct UserUnit {
    cfg: AutostartConfig,
    config_home: PathBuf,
    runtime_dir: Option<PathBuf>,
}

/// systemd 的 ExecStart 里给路径加引号（路径可能有空格）。
fn quote(p: &Path) -> String {
    let s = p.display().to_string();
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 单元文件全文。
pub fn unit_text(program: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=iWork background service\n\
         StartLimitIntervalSec=300\n\
         StartLimitBurst=10\n\
         \n\
         [Service]\n\
         ExecStart={} {SERVICE_ARG}\n\
         Restart=on-failure\n\
         RestartSec=10\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        quote(program)
    )
}

/// `ExecStart` 拆成程序与参数（只认本模块写出的形状：带引号的程序 + 空格分隔的参数）。
pub fn exec_start(text: &str) -> Option<(PathBuf, Vec<String>)> {
    let line = text.lines().find_map(|l| l.strip_prefix("ExecStart="))?;
    let rest = line.strip_prefix('"')?;
    let mut program = String::new();
    let mut chars = rest.chars();
    loop {
        match chars.next()? {
            '\\' => program.push(chars.next()?),
            '"' => break,
            c => program.push(c),
        }
    }
    let args = chars
        .as_str()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    Some((PathBuf::from(program), args))
}

impl UserUnit {
    pub fn new(cfg: AutostartConfig) -> Self {
        let env = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let config_home = env("XDG_CONFIG_HOME").unwrap_or_else(|| cfg.home.join(".config"));
        Self {
            config_home,
            runtime_dir: env("XDG_RUNTIME_DIR"),
            cfg,
        }
    }

    /// 测试用：指定配置目录与运行时目录，不读环境变量。
    pub fn with_dirs(
        cfg: AutostartConfig,
        config_home: PathBuf,
        runtime_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            cfg,
            config_home,
            runtime_dir,
        }
    }

    pub fn unit_path(&self) -> PathBuf {
        self.config_home.join("systemd/user").join(UNIT)
    }

    fn tool(&self) -> PathBuf {
        self.cfg
            .tool
            .clone()
            .unwrap_or_else(|| PathBuf::from("systemctl"))
    }

    async fn systemctl(&self, args: &[&str]) -> Result<(), AutostartError> {
        let mut all = vec!["--user"];
        all.extend_from_slice(args);
        let out = run(&self.tool(), &all).await?;
        if out.status.success() {
            Ok(())
        } else {
            Err(failed(&self.tool(), &all, &out))
        }
    }
}

impl AutostartPort for UserUnit {
    async fn state(&self) -> Result<AutostartState, AutostartError> {
        match std::fs::read_to_string(self.unit_path()) {
            Ok(text) => Ok(AutostartState::Registered {
                program: exec_start(&text).map(|(p, _)| p).unwrap_or_default(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AutostartState::NotRegistered),
            Err(e) => Err(AutostartError::Failed {
                message: format!("读 {} 失败：{e}", self.unit_path().display()),
            }),
        }
    }

    /// 有没有 systemd 用户实例：`$XDG_RUNTIME_DIR/systemd/private` 是用户实例的控制 socket。
    fn supported(&self) -> Result<(), AutostartError> {
        if self.cfg.assume_supported {
            return Ok(());
        }
        let has = self
            .runtime_dir
            .as_ref()
            .is_some_and(|d| d.join("systemd/private").exists());
        if has {
            Ok(())
        } else {
            Err(AutostartError::Unsupported {
                reason: "没有 systemd 用户实例".into(),
            })
        }
    }

    async fn set(&self, enabled: bool) -> Result<(), AutostartError> {
        self.supported()?;
        let path = self.unit_path();
        let want = unit_text(&self.cfg.program);
        let current = std::fs::read_to_string(&path).ok();
        if enabled {
            if current.as_deref() == Some(want.as_str()) {
                return Ok(());
            }
            write_atomically(&path, &want)?;
            self.systemctl(&["daemon-reload"]).await?;
            self.systemctl(&["enable", "--now", UNIT]).await
        } else {
            if current.is_none() {
                return Ok(());
            }
            // 从托管实例自己调用时，disable --now 会结束本进程（调用方只在 bind 之前这么做）
            self.systemctl(&["disable", "--now", UNIT]).await?;
            remove(&path)?;
            self.systemctl(&["daemon-reload"]).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autostart::test_tool::FakeTool;

    fn unit(tool: &FakeTool, program: &str, runtime: Option<PathBuf>, assume: bool) -> UserUnit {
        UserUnit::with_dirs(
            AutostartConfig {
                program: PathBuf::from(program),
                home: tool.home().join("home"),
                tool: Some(tool.path()),
                assume_supported: assume,
            },
            tool.home().join("config"),
            runtime,
        )
    }

    #[tokio::test]
    async fn linux_register_writes_unit_and_enables() {
        let t = FakeTool::new();
        let u = unit(&t, "/opt/iwork/iwork-daemon", None, true);
        u.set(true).await.unwrap();
        let text = std::fs::read_to_string(u.unit_path()).unwrap();
        assert!(text.contains("Restart=on-failure"));
        assert!(!text.contains("RestartForceExitStatus"), "N4：不加");
        assert!(text.contains("WantedBy=default.target"));
        assert_eq!(
            exec_start(&text).unwrap(),
            (
                PathBuf::from("/opt/iwork/iwork-daemon"),
                vec![SERVICE_ARG.to_owned()]
            )
        );
        assert_eq!(
            t.calls(),
            [
                "--user daemon-reload",
                "--user enable --now iwork-daemon.service"
            ]
        );
        assert_eq!(
            u.state().await.unwrap(),
            AutostartState::Registered {
                program: PathBuf::from("/opt/iwork/iwork-daemon")
            }
        );
    }

    #[tokio::test]
    async fn linux_remove_disables_and_deletes() {
        let t = FakeTool::new();
        let u = unit(&t, "/opt/iwork/iwork-daemon", None, true);
        u.set(true).await.unwrap();
        u.set(false).await.unwrap();
        assert!(!u.unit_path().exists());
        assert_eq!(u.state().await.unwrap(), AutostartState::NotRegistered);
        assert_eq!(
            &t.calls()[2..],
            [
                "--user disable --now iwork-daemon.service",
                "--user daemon-reload"
            ]
        );
    }

    #[tokio::test]
    async fn linux_set_is_idempotent() {
        let t = FakeTool::new();
        let u = unit(&t, "/opt/iwork/iwork-daemon", None, true);
        u.set(true).await.unwrap();
        u.set(true).await.unwrap();
        assert_eq!(t.calls().len(), 2);
        u.set(false).await.unwrap();
        u.set(false).await.unwrap();
        assert_eq!(t.calls().len(), 4);
    }

    /// 路径有空格、变了 → 重写。
    #[tokio::test]
    async fn linux_path_change_rewrites() {
        let t = FakeTool::new();
        unit(&t, "/old/iwork-daemon", None, true)
            .set(true)
            .await
            .unwrap();
        let moved = unit(&t, "/new place/iwork-daemon", None, true);
        moved.set(true).await.unwrap();
        assert_eq!(
            moved.state().await.unwrap(),
            AutostartState::Registered {
                program: PathBuf::from("/new place/iwork-daemon")
            }
        );
    }

    /// 没有 systemd 用户实例 → Unsupported，什么都不写、不调。对照：有控制 socket 时支持。
    #[tokio::test]
    async fn linux_without_user_instance_is_unsupported() {
        let t = FakeTool::new();
        let u = unit(&t, "/x", Some(t.home().join("rt")), false);
        assert!(matches!(
            u.supported(),
            Err(AutostartError::Unsupported { .. })
        ));
        assert!(matches!(
            u.set(true).await,
            Err(AutostartError::Unsupported { .. })
        ));
        assert!(!u.unit_path().exists());
        assert!(t.calls().is_empty());
        std::fs::create_dir_all(t.home().join("rt/systemd")).unwrap();
        std::fs::write(t.home().join("rt/systemd/private"), b"").unwrap();
        assert!(u.supported().is_ok());
    }
}
