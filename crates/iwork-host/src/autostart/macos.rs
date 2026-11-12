//! macOS：LaunchAgent plist + `launchctl bootstrap gui/<uid>` / `bootout`（T04 方案 2.12；N1 实测）。
//!
//! - plist 写 `Program` = 包内后台程序的绝对路径，`ProgramArguments` 带 `--service`；不写 `BundleProgram`
//!   （放在 `~/Library/LaunchAgents` 用 bootstrap 加载时一律 rc=5，N1 第 10 项）。
//! - `KeepAlive = { SuccessfulExit = false }`：非零退出或被信号结束约 10s 后再拉，退出状态 0 不拉（N1 第 6 项）。
//! - app 挪了位置：注册项里的路径和现在的不一样 → 对齐任务调 `set(true)` 重写（`state` 报出旧路径）。
//! - `bootstrap` 立刻起一个实例；`bootout` 结束正在跑的实例（从实例自己调用时调用不返回，N1 第 5–7 项）。
//! - 状态两半：plist 在不在 + `launchctl print gui/<uid>/<label>` 认不认（已加载）。注销先删 plist 再 bootout，
//!   被 bootout 结束在半路也只剩「已加载」那一半：下次登录不会再起，下次对齐查到 `Partial` 再 bootout。

use std::path::{Path, PathBuf};

use iwork_ports::autostart::{AutostartError, AutostartPort, AutostartState};

use super::{
    AutostartConfig, Live, SERVICE_ARG, combine, failed, read_definition, remove, run,
    write_atomically,
};

/// launchd 的标签。
pub const LABEL: &str = "com.iwork.daemon";

pub struct LaunchAgent {
    cfg: AutostartConfig,
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
}

/// plist 全文。
pub fn plist_text(program: &Path) -> String {
    let p = xml_escape(&program.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>Program</key><string>{p}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{p}</string>
    <string>{SERVICE_ARG}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key><false/>
  </dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#
    )
}

/// plist 里 `ProgramArguments` 的参数（不含程序本身）。打包检查用它核对「参数能被后台解析」。
pub fn plist_args(text: &str) -> Vec<String> {
    let Some(start) = text.find("<key>ProgramArguments</key>") else {
        return Vec::new();
    };
    let rest = &text[start..];
    let end = rest.find("</array>").unwrap_or(rest.len());
    rest[..end]
        .split("<string>")
        .skip(1)
        .filter_map(|s| s.split("</string>").next())
        .map(xml_unescape)
        .skip(1)
        .collect()
}

/// plist 里 `Program` 的值。
fn plist_program(text: &str) -> Option<PathBuf> {
    let start = text.find("<key>Program</key>")? + "<key>Program</key>".len();
    let rest = &text[start..];
    let open = rest.find("<string>")? + "<string>".len();
    let close = rest[open..].find("</string>")?;
    Some(PathBuf::from(xml_unescape(&rest[open..open + close])))
}

impl LaunchAgent {
    pub fn new(cfg: AutostartConfig) -> Self {
        Self { cfg }
    }

    pub fn plist_path(&self) -> PathBuf {
        self.cfg
            .home
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    }

    fn tool(&self) -> Result<PathBuf, AutostartError> {
        self.cfg.command("/bin/launchctl")
    }

    fn domain(&self) -> String {
        // SAFETY：getuid 总是成功、不改任何东西
        format!("gui/{}", unsafe { libc::getuid() })
    }

    fn target(&self) -> String {
        format!("{}/{LABEL}", self.domain())
    }

    /// 服务管理器那一半：`launchctl print` 成功就是已载入；实际载入的程序取输出里 `program = …` 那一行
    /// （取不到为 `None`，按「不符」处理）。
    async fn live(&self) -> Result<Live, AutostartError> {
        let target = self.target();
        let out = run(&self.tool()?, &["print", target.as_str()]).await?;
        if !out.status.success() {
            return Ok(Live::Absent);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let program = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("program = "))
            .map(PathBuf::from);
        Ok(Live::Loaded(program))
    }

    /// `bootout`：服务没加载（之前没 bootstrap 过、或已被结束）不算错。
    async fn bootout(&self) -> Result<(), AutostartError> {
        let tool = self.tool()?;
        let target = self.target();
        let args = ["bootout", target.as_str()];
        let out = run(&tool, &args).await?;
        if out.status.success() {
            return Ok(());
        }
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        // launchctl：没加载时报「No such process」/「Could not find service」（退出码 3 或 113）
        if text.contains("No such process") || text.contains("Could not find service") {
            return Ok(());
        }
        Err(failed(&tool, &args, &out))
    }
}

impl AutostartPort for LaunchAgent {
    async fn state(&self) -> Result<AutostartState, AutostartError> {
        if self.cfg.disabled() {
            return Ok(AutostartState::NotRegistered);
        }
        // 读得到但认不出 Program：当作注册着一个不对的东西（空路径），让 set(true) 重写
        let definition = read_definition(&self.plist_path())?
            .map(|text| plist_program(&text).unwrap_or_default());
        Ok(combine(definition, self.live().await?))
    }

    fn supported(&self) -> Result<(), AutostartError> {
        self.tool().map(|_| ())
    }

    async fn set(&self, enabled: bool) -> Result<(), AutostartError> {
        let tool = self.tool()?;
        let path = self.plist_path();
        let current = read_definition(&path)?;
        let live = self.live().await?;
        if enabled {
            let want = plist_text(&self.cfg.program);
            let same = current.as_deref() == Some(want.as_str());
            // 已对齐 = 文件是这一份，并且实际载入的也是这个程序（组 3 审查 2/F-1）
            if same && live == Live::Loaded(Some(self.cfg.program.clone())) {
                return Ok(());
            }
            if !same {
                write_atomically(&path, &want)?;
            }
            if live != Live::Absent {
                // 载着的不是这一份（app 挪了位置、上次更新到一半）：先卸下。失败就返回错误，state 仍报 Partial，
                // 下一轮重做。托管实例不走这里（对齐表「开 / 定义不符 / 托管」只调 write_definition）
                self.bootout().await?;
            }
            let domain = self.domain();
            let plist = path.display().to_string();
            let args = ["bootstrap", domain.as_str(), plist.as_str()];
            let out = run(&tool, &args).await?;
            if !out.status.success() {
                // plist 留着、没载入：state 报 Partial，下次对齐再 bootstrap
                return Err(failed(&tool, &args, &out));
            }
            Ok(())
        } else {
            if current.is_none() && live == Live::Absent {
                return Ok(());
            }
            // 先删 plist 再 bootout：从托管实例自己调用时 bootout 会结束本进程、不返回，
            // 那时 plist 已经不在，下次登录不会再起
            remove(&path)?;
            if live != Live::Absent {
                self.bootout().await?;
            }
            Ok(())
        }
    }

    async fn write_definition(&self) -> Result<(), AutostartError> {
        self.tool()?;
        let path = self.plist_path();
        let want = plist_text(&self.cfg.program);
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

    fn agent(tool: &FakeTool, program: &str) -> LaunchAgent {
        LaunchAgent::new(AutostartConfig {
            program: PathBuf::from(program),
            home: tool.home().join("home"),
            tool: Tool::Substitute(tool.path()),
            assume_supported: false,
        })
    }

    #[tokio::test]
    async fn macos_register_writes_program_plist_and_bootstraps() {
        let t = FakeTool::new();
        let a = agent(&t, "/Applications/iWork.app/Contents/MacOS/iwork-daemon");
        assert_eq!(a.state().await.unwrap(), AutostartState::NotRegistered);
        a.set(true).await.unwrap();
        let text = std::fs::read_to_string(a.plist_path()).unwrap();
        assert!(text.contains("<key>Program</key>"));
        assert!(
            !text.contains("BundleProgram"),
            "不用 BundleProgram（N1 第 10 项）"
        );
        assert!(text.contains("<key>SuccessfulExit</key><false/>"));
        assert_eq!(plist_args(&text), [SERVICE_ARG]);
        assert_eq!(
            a.state().await.unwrap(),
            AutostartState::Registered {
                program: PathBuf::from("/Applications/iWork.app/Contents/MacOS/iwork-daemon")
            }
        );
        let calls = t.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].starts_with("bootstrap gui/"), "{calls:?}");
        assert!(calls[0].ends_with("com.iwork.daemon.plist"), "{calls:?}");
    }

    #[tokio::test]
    async fn macos_set_is_idempotent() {
        let t = FakeTool::new();
        let a = agent(&t, "/x/iwork-daemon");
        a.set(true).await.unwrap();
        a.set(true).await.unwrap();
        assert_eq!(t.calls().len(), 1, "已是目标状态时什么都不做");
        a.set(false).await.unwrap();
        a.set(false).await.unwrap();
        let calls = t.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[1].starts_with("bootout gui/") && calls[1].ends_with("/com.iwork.daemon"));
        assert!(!a.plist_path().exists());
        assert_eq!(a.state().await.unwrap(), AutostartState::NotRegistered);
    }

    /// app 挪了位置：注册项指向旧路径 → set(true) 卸下旧的、写新路径、重新 bootstrap。
    #[tokio::test]
    async fn macos_path_change_rewrites() {
        let t = FakeTool::new();
        agent(&t, "/old/iwork-daemon").set(true).await.unwrap();
        let moved = agent(&t, "/new place/iwork-daemon");
        assert_eq!(
            moved.state().await.unwrap(),
            AutostartState::Registered {
                program: PathBuf::from("/old/iwork-daemon")
            }
        );
        moved.set(true).await.unwrap();
        assert_eq!(
            moved.state().await.unwrap(),
            AutostartState::Registered {
                program: PathBuf::from("/new place/iwork-daemon")
            }
        );
        let calls = t.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert!(calls[1].starts_with("bootout"));
        assert!(calls[2].starts_with("bootstrap"));
    }

    /// bootstrap 失败 → Failed，带命令与输出；plist 写下了但没加载，state 报 Partial（不是 Registered），
    /// 下次 set(true) 只补 bootstrap。
    #[tokio::test]
    async fn macos_bootstrap_failure_is_partial_and_retried() {
        let t = FakeTool::new();
        let a = agent(&t, "/x/iwork-daemon");
        t.fail_on("bootstrap");
        let e = a.set(true).await.unwrap_err();
        assert!(
            matches!(&e, AutostartError::Failed { message } if message.contains("bootstrap")),
            "{e}"
        );
        assert!(a.plist_path().exists());
        assert_eq!(a.state().await.unwrap(), AutostartState::Partial);
        t.stop_failing();
        a.set(true).await.unwrap();
        assert!(t.live());
        assert_eq!(
            a.state().await.unwrap(),
            AutostartState::Registered {
                program: PathBuf::from("/x/iwork-daemon")
            }
        );
        let calls = t.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(
            calls.iter().all(|c| c.starts_with("bootstrap")),
            "{calls:?}"
        );
    }

    /// 服务在 iWork 之外被卸下（plist 还在）→ Partial；set(true) 重新 bootstrap。
    #[tokio::test]
    async fn macos_unloaded_outside_is_partial_and_reloaded() {
        let t = FakeTool::new();
        let a = agent(&t, "/x/iwork-daemon");
        a.set(true).await.unwrap();
        t.drop_live();
        assert_eq!(a.state().await.unwrap(), AutostartState::Partial);
        a.set(true).await.unwrap();
        assert!(t.live());
    }

    /// 注销先删 plist 再 bootout：bootout 没完成（托管实例自己调用时被结束；这里用失败模拟）时 plist 已经不在，
    /// 下次登录不会再起；state 报 Partial，再调一次 set(false) 补上 bootout。
    #[tokio::test]
    async fn macos_unregister_removes_plist_before_bootout() {
        let t = FakeTool::new();
        let a = agent(&t, "/x/iwork-daemon");
        a.set(true).await.unwrap();
        t.fail_on("bootout");
        a.set(false).await.unwrap_err();
        assert!(!a.plist_path().exists(), "bootout 之前 plist 已删");
        assert_eq!(a.state().await.unwrap(), AutostartState::Partial);
        t.stop_failing();
        a.set(false).await.unwrap();
        assert!(!t.live());
        assert_eq!(a.state().await.unwrap(), AutostartState::NotRegistered);
    }

    /// 2/F-1：更新已注册的定义时 bootout 失败 → 新 plist 写下了、实际载入的还是旧程序：不算对齐（Partial），
    /// 下一轮 set(true) 不早退，补上 bootout 与 bootstrap。
    #[tokio::test]
    async fn macos_failed_bootout_on_update_is_not_aligned() {
        let t = FakeTool::new();
        agent(&t, "/old/iwork-daemon").set(true).await.unwrap();
        let moved = agent(&t, "/new/iwork-daemon");
        t.fail_on("bootout");
        moved.set(true).await.unwrap_err();
        assert_eq!(
            plist_program(&std::fs::read_to_string(moved.plist_path()).unwrap()),
            Some(PathBuf::from("/new/iwork-daemon")),
            "前提：新 plist 已写下"
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
        let calls = t.calls();
        assert!(calls.last().unwrap().starts_with("bootstrap"), "{calls:?}");
    }

    /// 2/F-2：只写定义文件：plist 换成现在这个程序，不 bootout、不 bootstrap；实际载入的仍是旧的（Partial）。
    #[tokio::test]
    async fn macos_write_definition_does_not_touch_launchd() {
        let t = FakeTool::new();
        agent(&t, "/old/iwork-daemon").set(true).await.unwrap();
        let moved = agent(&t, "/new/iwork-daemon");
        moved.write_definition().await.unwrap();
        moved.write_definition().await.unwrap();
        assert_eq!(
            t.calls().len(),
            1,
            "只有最初那次 bootstrap：{:?}",
            t.calls()
        );
        assert_eq!(
            plist_program(&std::fs::read_to_string(moved.plist_path()).unwrap()),
            Some(PathBuf::from("/new/iwork-daemon"))
        );
        assert_eq!(moved.state().await.unwrap(), AutostartState::Partial);
    }

    /// 2/F-4：替身禁用（开发构建没给替身）→ 不支持、报没注册、set 与 write_definition 都报错，不写文件。
    #[tokio::test]
    async fn macos_disabled_tool_does_nothing() {
        let t = FakeTool::new();
        let a = LaunchAgent::new(AutostartConfig {
            program: PathBuf::from("/x/iwork-daemon"),
            home: t.home().join("home"),
            tool: Tool::Disabled,
            assume_supported: false,
        });
        assert!(matches!(
            a.supported(),
            Err(AutostartError::Unsupported { .. })
        ));
        assert_eq!(a.state().await.unwrap(), AutostartState::NotRegistered);
        assert!(a.set(true).await.is_err());
        assert!(a.set(false).await.is_err());
        assert!(a.write_definition().await.is_err());
        assert!(!a.plist_path().exists());
    }

    /// 2/F-4：测试构建里用到系统的 launchctl 直接 panic（测试只隔离了 HOME，没隔离 launchd）。
    #[tokio::test]
    #[should_panic(expected = "测试构建不许运行系统的")]
    async fn macos_system_tool_is_refused_in_tests() {
        let t = FakeTool::new();
        let a = LaunchAgent::new(AutostartConfig {
            program: PathBuf::from("/x/iwork-daemon"),
            home: t.home().join("home"),
            tool: Tool::System,
            assume_supported: false,
        });
        let _ = a.state().await;
    }

    /// 定义文件的位置上是一个指向别处的符号链接 → 拒绝写，链接指向的文件不变。
    #[tokio::test]
    async fn macos_symlinked_plist_is_not_followed() {
        let t = FakeTool::new();
        let a = agent(&t, "/x/iwork-daemon");
        let victim = t.home().join("victim");
        std::fs::write(&victim, "keep me").unwrap();
        std::fs::create_dir_all(a.plist_path().parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&victim, a.plist_path()).unwrap();
        a.set(true).await.unwrap_err();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
    }

    #[test]
    fn plist_escapes_paths() {
        let text = plist_text(Path::new("/a&b/<x>/iwork-daemon"));
        assert_eq!(
            plist_program(&text).unwrap(),
            PathBuf::from("/a&b/<x>/iwork-daemon")
        );
    }
}
