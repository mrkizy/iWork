//! Linux：systemd 用户单元（T04 方案 2.12；N4 实测）。
//!
//! - `$XDG_CONFIG_HOME/systemd/user/iwork-daemon.service`：`ExecStart=<绝对路径> --service`、`Restart=on-failure`、
//!   `RestartSec=10`、`StartLimitIntervalSec=300`、`StartLimitBurst=10`、`WantedBy=default.target`。
//! - **不加 `RestartForceExitStatus=SIGTERM`**：被要求停下（SIGTERM 等）不自动再拉，systemd 的默认正合这个意思。
//!   和 launchd 只在后台装上信号处理之前那一小段不一样（方案 2.12 如实写了）。
//! - 状态两半：单元文件在不在 + `systemctl --user show` 的 `UnitFileState`（启用没有）、`NeedDaemonReload`（文件改了还没
//!   重新载入）、`ExecStart`（实际载入的程序）。
//! - 开：写单元、`daemon-reload`、`enable --now`（已在跑时不重启）。
//! - 关：`disable`（只撤启用、不停）、删文件、`daemon-reload`，最后 `stop`。`stop` 会结束服务名下的实例——可能就是
//!   调用者自己，那时定义文件和启用都已经撤掉，下次登录不会再起。
//! - 没有 systemd 用户实例 → `Unsupported`。

use std::path::{Path, PathBuf};

use iwork_ports::autostart::{AutostartError, AutostartPort, AutostartState};

use super::{
    AutostartConfig, Live, SERVICE_ARG, combine, failed, read_definition, remove, run,
    write_atomically,
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

    fn tool(&self) -> Result<PathBuf, AutostartError> {
        self.cfg.command("systemctl")
    }

    /// 服务管理器那一半：启用了没有，启用了的话实际载入的是哪个程序。单元文件改过而还没 `daemon-reload`
    /// （`NeedDaemonReload=yes`），或 `ExecStart` 取不到，都算「实际载入的不知道是哪份」（`None`，按不符处理）。
    async fn live(&self) -> Result<Live, AutostartError> {
        let out = run(
            &self.tool()?,
            &[
                "--user",
                "show",
                "-p",
                "UnitFileState,NeedDaemonReload,ExecStart",
                UNIT,
            ],
        )
        .await?;
        Ok(parse_show(&String::from_utf8_lossy(&out.stdout)))
    }

    /// `stop`：单元已经不认识（没在跑、文件也删了）不算错。
    async fn stop(&self) -> Result<(), AutostartError> {
        let tool = self.tool()?;
        let args = ["--user", "stop", UNIT];
        let out = run(&tool, &args).await?;
        if out.status.success() || String::from_utf8_lossy(&out.stderr).contains("not loaded") {
            Ok(())
        } else {
            Err(failed(&tool, &args, &out))
        }
    }

    async fn systemctl(&self, args: &[&str]) -> Result<(), AutostartError> {
        let tool = self.tool()?;
        let mut all = vec!["--user"];
        all.extend_from_slice(args);
        let out = run(&tool, &all).await?;
        if out.status.success() {
            Ok(())
        } else {
            Err(failed(&tool, &all, &out))
        }
    }
}

/// `systemctl show -p UnitFileState,NeedDaemonReload,ExecStart` 的输出 → 服务管理器那一半。
fn parse_show(text: &str) -> Live {
    let value = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or("")
    };
    if value("UnitFileState") != "enabled" {
        return Live::Absent;
    }
    if value("NeedDaemonReload") != "no" {
        return Live::Loaded(None);
    }
    // ExecStart={ path=/opt/iwork/iwork-daemon ; argv[]=… ; … }
    let program = value("ExecStart")
        .split_once("path=")
        .and_then(|(_, rest)| rest.split_once(" ;"))
        .map(|(p, _)| PathBuf::from(p));
    Live::Loaded(program)
}

impl AutostartPort for UserUnit {
    async fn state(&self) -> Result<AutostartState, AutostartError> {
        if self.cfg.disabled() {
            return Ok(AutostartState::NotRegistered);
        }
        let definition = read_definition(&self.unit_path())?
            .map(|text| exec_start(&text).map(|(p, _)| p).unwrap_or_default());
        Ok(combine(definition, self.live().await?))
    }

    /// 有没有 systemd 用户实例：`$XDG_RUNTIME_DIR/systemd/private` 是用户实例的控制 socket。
    fn supported(&self) -> Result<(), AutostartError> {
        self.tool()?;
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
        let live = self.live().await?;
        if enabled {
            let want = unit_text(&self.cfg.program);
            let same = current.as_deref() == Some(want.as_str());
            // 已对齐 = 文件是这一份，启用了，并且实际载入的也是这个程序（组 3 审查 2/F-1）
            if same && live == Live::Loaded(Some(self.cfg.program.clone())) {
                return Ok(());
            }
            if !same {
                write_atomically(&path, &want)?;
            }
            // 任何一步失败都返回错误：state 仍报 Partial（没启用，或 NeedDaemonReload=yes），下次对齐从这里再来。
            // `enable --now` 对已在跑的实例不重启，不会结束调用者
            self.systemctl(&["daemon-reload"]).await?;
            self.systemctl(&["enable", "--now", UNIT]).await
        } else {
            if current.is_none() && live == Live::Absent {
                return Ok(());
            }
            if live != Live::Absent {
                self.systemctl(&["disable", UNIT]).await?;
            }
            remove(&path)?;
            self.systemctl(&["daemon-reload"]).await?;
            // 最后才停：从托管实例自己调用时在这里被结束，定义文件与启用都已撤掉
            self.stop().await
        }
    }

    async fn write_definition(&self) -> Result<(), AutostartError> {
        self.supported()?;
        let path = self.unit_path();
        let want = unit_text(&self.cfg.program);
        if read_definition(&path)?.as_deref() != Some(want.as_str()) {
            write_atomically(&path, &want)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autostart::Tool;
    use crate::autostart::test_tool::FakeTool;

    fn unit(tool: &FakeTool, program: &str, runtime: Option<PathBuf>, assume: bool) -> UserUnit {
        UserUnit::with_dirs(
            AutostartConfig {
                program: PathBuf::from(program),
                home: tool.home().join("home"),
                tool: Tool::Substitute(tool.path()),
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

    /// 2/F-1：更新已注册的定义时 daemon-reload 失败 → 新单元写下了、旧的还启用着（NeedDaemonReload=yes）：
    /// 不算对齐（Partial），下一轮 set(true) 不早退，补上 reload 与 enable。
    #[tokio::test]
    async fn linux_failed_reload_on_update_is_not_aligned() {
        let t = FakeTool::new();
        unit(&t, "/old/iwork-daemon", None, true)
            .set(true)
            .await
            .unwrap();
        let moved = unit(&t, "/new/iwork-daemon", None, true);
        t.fail_on("daemon-reload");
        moved.set(true).await.unwrap_err();
        assert!(
            std::fs::read_to_string(moved.unit_path())
                .unwrap()
                .contains("/new/iwork-daemon"),
            "前提：新单元已写下"
        );
        assert_eq!(moved.state().await.unwrap(), AutostartState::Partial);
        t.stop_failing();
        moved.set(true).await.unwrap();
        assert_eq!(
            moved.state().await.unwrap(),
            AutostartState::Registered {
                program: PathBuf::from("/new/iwork-daemon")
            }
        );
        assert_eq!(
            &t.calls()[t.calls().len() - 2..],
            [
                "--user daemon-reload",
                "--user enable --now iwork-daemon.service"
            ]
        );
    }

    /// 2/F-2：只写定义文件：单元换成现在这个程序，不 reload、不 enable、不 stop。
    #[tokio::test]
    async fn linux_write_definition_does_not_touch_systemd() {
        let t = FakeTool::new();
        unit(&t, "/old/iwork-daemon", None, true)
            .set(true)
            .await
            .unwrap();
        let moved = unit(&t, "/new/iwork-daemon", None, true);
        moved.write_definition().await.unwrap();
        assert_eq!(
            t.calls().len(),
            2,
            "只有最初的 reload 与 enable：{:?}",
            t.calls()
        );
        assert!(
            std::fs::read_to_string(moved.unit_path())
                .unwrap()
                .contains("/new/iwork-daemon")
        );
        assert_eq!(moved.state().await.unwrap(), AutostartState::Partial);
    }

    /// `systemctl show` 的解析：没启用 → 不在；文件改了没重新载入 → 载入的不知道是哪份；否则取 ExecStart 的 path。
    #[test]
    fn parse_show_table() {
        assert_eq!(
            parse_show("UnitFileState=disabled\nNeedDaemonReload=no\n"),
            Live::Absent
        );
        assert_eq!(parse_show(""), Live::Absent);
        assert_eq!(
            parse_show(
                "UnitFileState=enabled\nNeedDaemonReload=yes\nExecStart={ path=/a ; argv[]=/a ; }\n"
            ),
            Live::Loaded(None)
        );
        assert_eq!(
            parse_show(
                "UnitFileState=enabled\nNeedDaemonReload=no\nExecStart={ path=/new place/iwork-daemon ; argv[]=/new place/iwork-daemon --service ; ignore_errors=no ; }\n"
            ),
            Live::Loaded(Some(PathBuf::from("/new place/iwork-daemon")))
        );
        assert_eq!(
            parse_show("UnitFileState=enabled\nNeedDaemonReload=no\nExecStart=\n"),
            Live::Loaded(None)
        );
    }

    /// 2/F-4：替身禁用 → 不支持、报没注册，什么命令都不跑、不写文件。
    #[tokio::test]
    async fn linux_disabled_tool_does_nothing() {
        let t = FakeTool::new();
        let mut u = unit(&t, "/x", None, true);
        u.cfg.tool = Tool::Disabled;
        assert!(matches!(
            u.supported(),
            Err(AutostartError::Unsupported { .. })
        ));
        assert_eq!(u.state().await.unwrap(), AutostartState::NotRegistered);
        assert!(u.set(true).await.is_err());
        assert!(u.write_definition().await.is_err());
        assert!(!u.unit_path().exists());
        assert!(t.calls().is_empty());
    }

    /// 2/F-4：测试构建里用到系统的 systemctl 直接 panic。
    #[tokio::test]
    #[should_panic(expected = "测试构建不许运行系统的")]
    async fn linux_system_tool_is_refused_in_tests() {
        let t = FakeTool::new();
        let mut u = unit(&t, "/x", None, true);
        u.cfg.tool = Tool::System;
        let _ = u.state().await;
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
