//! Linux：systemd 用户单元（T04 方案 2.12；N4 实测）。
//!
//! - `$XDG_CONFIG_HOME/systemd/user/iwork-daemon.service`：`ExecStart=<绝对路径> --service`、`Restart=on-failure`、
//!   `RestartSec=10`、`StartLimitIntervalSec=300`、`StartLimitBurst=10`、`WantedBy=default.target`。
//! - **不加 `RestartForceExitStatus=SIGTERM`**：被要求停下（SIGTERM 等）不自动再拉，systemd 的默认正合这个意思。
//!   和 launchd 只在后台装上信号处理之前那一小段不一样（方案 2.12 如实写了）。
//! - 状态两半：单元文件在不在 + `systemctl --user is-enabled` 是不是 `enabled`。
//! - 开：写单元、`daemon-reload`、`enable --now`（已在跑时不重启）。
//! - 关：`disable`（只撤启用、不停）、删文件、`daemon-reload`，最后 `stop`。`stop` 会结束服务名下的实例——可能就是
//!   调用者自己，那时定义文件和启用都已经撤掉，下次登录不会再起。
//! - 没有 systemd 用户实例 → `Unsupported`。

use std::path::{Path, PathBuf};

use iwork_ports::autostart::{AutostartError, AutostartPort, AutostartState};

use super::{
    AutostartConfig, SERVICE_ARG, combine, failed, read_definition, remove, run, write_atomically,
};

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

    /// 服务管理器那一半：`is-enabled` 报 `enabled`（禁用、不认识这个单元时报别的，并以非零退出）。
    async fn enabled(&self) -> Result<bool, AutostartError> {
        let out = run(&self.tool(), &["--user", "is-enabled", UNIT]).await?;
        Ok(String::from_utf8_lossy(&out.stdout).trim() == "enabled")
    }

    /// `stop`：单元已经不认识（没在跑、文件也删了）不算错。
    async fn stop(&self) -> Result<(), AutostartError> {
        let args = ["--user", "stop", UNIT];
        let out = run(&self.tool(), &args).await?;
        if out.status.success() || String::from_utf8_lossy(&out.stderr).contains("not loaded") {
            Ok(())
        } else {
            Err(failed(&self.tool(), &args, &out))
        }
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
        let definition = read_definition(&self.unit_path())?
            .map(|text| exec_start(&text).map(|(p, _)| p).unwrap_or_default());
        Ok(combine(definition, self.enabled().await?))
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
        let current = read_definition(&path)?;
        let live = self.enabled().await?;
        if enabled {
            let want = unit_text(&self.cfg.program);
            let same = current.as_deref() == Some(want.as_str());
            if same && live {
                return Ok(());
            }
            if !same {
                write_atomically(&path, &want)?;
            }
            // 失败时单元文件留着、没启用：state 报 Partial，下次对齐从这里再来
            self.systemctl(&["daemon-reload"]).await?;
            self.systemctl(&["enable", "--now", UNIT]).await
        } else {
            if current.is_none() && !live {
                return Ok(());
            }
            if live {
                self.systemctl(&["disable", UNIT]).await?;
            }
            remove(&path)?;
            self.systemctl(&["daemon-reload"]).await?;
            // 最后才停：从托管实例自己调用时在这里被结束，定义文件与启用都已撤掉
            self.stop().await
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
                "--user disable iwork-daemon.service",
                "--user daemon-reload",
                "--user stop iwork-daemon.service"
            ]
        );
    }

    /// 注销时 stop 没完成（托管实例自己调用时被结束；这里用失败模拟）：单元文件与启用都已撤掉，下次登录不会再起。
    #[tokio::test]
    async fn linux_unregister_withdraws_before_stop() {
        let t = FakeTool::new();
        let u = unit(&t, "/opt/iwork/iwork-daemon", None, true);
        u.set(true).await.unwrap();
        t.fail_on("stop");
        u.set(false).await.unwrap_err();
        assert!(!u.unit_path().exists());
        assert!(!t.live());
        assert_eq!(u.state().await.unwrap(), AutostartState::NotRegistered);
    }

    /// enable 失败 → 单元文件在、没启用：state 报 Partial（不是 Registered）；下次 set(true) 补 reload 与 enable。
    #[tokio::test]
    async fn linux_enable_failure_is_partial_and_retried() {
        let t = FakeTool::new();
        let u = unit(&t, "/opt/iwork/iwork-daemon", None, true);
        t.fail_on("enable");
        u.set(true).await.unwrap_err();
        assert!(u.unit_path().exists());
        assert_eq!(u.state().await.unwrap(), AutostartState::Partial);
        t.stop_failing();
        u.set(true).await.unwrap();
        assert_eq!(
            u.state().await.unwrap(),
            AutostartState::Registered {
                program: PathBuf::from("/opt/iwork/iwork-daemon")
            }
        );
        assert_eq!(
            &t.calls()[2..],
            [
                "--user daemon-reload",
                "--user enable --now iwork-daemon.service"
            ]
        );
    }

    /// 单元在 iWork 之外被禁用 → Partial；set(false) 不再 disable，照样删文件、reload、stop。
    #[tokio::test]
    async fn linux_disabled_outside_is_partial_and_cleaned() {
        let t = FakeTool::new();
        let u = unit(&t, "/opt/iwork/iwork-daemon", None, true);
        u.set(true).await.unwrap();
        t.drop_live();
        assert_eq!(u.state().await.unwrap(), AutostartState::Partial);
        u.set(false).await.unwrap();
        assert!(!u.unit_path().exists());
        assert_eq!(
            &t.calls()[2..],
            ["--user daemon-reload", "--user stop iwork-daemon.service"]
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
        assert_eq!(t.calls().len(), 5);
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
