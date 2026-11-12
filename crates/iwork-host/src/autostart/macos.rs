//! macOS：LaunchAgent plist + `launchctl bootstrap gui/<uid>` / `bootout`（T04 方案 2.12；N1 实测）。
//!
//! - plist 写 `Program` = 包内后台程序的绝对路径，`ProgramArguments` 带 `--service`；不写 `BundleProgram`
//!   （放在 `~/Library/LaunchAgents` 用 bootstrap 加载时一律 rc=5，N1 第 10 项）。
//! - `KeepAlive = { SuccessfulExit = false }`：非零退出或被信号结束约 10s 后再拉，退出状态 0 不拉（N1 第 6 项）。
//! - app 挪了位置：注册项里的路径和现在的不一样 → 对齐任务调 `set(true)` 重写（`state` 报出旧路径）。
//! - `bootstrap` 立刻起一个实例；`bootout` 结束正在跑的实例（从实例自己调用时调用不返回，N1 第 5–7 项）。

use std::path::{Path, PathBuf};

use iwork_ports::autostart::{AutostartError, AutostartPort, AutostartState};

use super::{AutostartConfig, SERVICE_ARG, failed, remove, run, write_atomically};

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

    fn tool(&self) -> PathBuf {
        self.cfg
            .tool
            .clone()
            .unwrap_or_else(|| PathBuf::from("/bin/launchctl"))
    }

    fn domain(&self) -> String {
        // SAFETY：getuid 总是成功、不改任何东西
        format!("gui/{}", unsafe { libc::getuid() })
    }

    /// `bootout`：服务没加载（之前没 bootstrap 过、或已被结束）不算错。
    async fn bootout(&self) -> Result<(), AutostartError> {
        let target = format!("{}/{LABEL}", self.domain());
        let args = ["bootout", target.as_str()];
        let out = run(&self.tool(), &args).await?;
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
        Err(failed(&self.tool(), &args, &out))
    }
}

impl AutostartPort for LaunchAgent {
    async fn state(&self) -> Result<AutostartState, AutostartError> {
        match std::fs::read_to_string(self.plist_path()) {
            Ok(text) => Ok(match plist_program(&text) {
                Some(program) => AutostartState::Registered { program },
                // 读得到但认不出 Program：当作注册着一个不对的东西，让 set(true) 重写
                None => AutostartState::Registered {
                    program: PathBuf::new(),
                },
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AutostartState::NotRegistered),
            Err(e) => Err(AutostartError::Failed {
                message: format!("读 {} 失败：{e}", self.plist_path().display()),
            }),
        }
    }

    fn supported(&self) -> Result<(), AutostartError> {
        Ok(())
    }

    async fn set(&self, enabled: bool) -> Result<(), AutostartError> {
        let path = self.plist_path();
        let want = plist_text(&self.cfg.program);
        let current = std::fs::read_to_string(&path).ok();
        if enabled {
            if current.as_deref() == Some(want.as_str()) {
                return Ok(());
            }
            if current.is_some() {
                // 旧的注册项（如 app 挪了位置）先卸下再换
                self.bootout().await?;
            }
            write_atomically(&path, &want)?;
            let domain = self.domain();
            let plist = path.display().to_string();
            let args = ["bootstrap", domain.as_str(), plist.as_str()];
            let out = run(&self.tool(), &args).await?;
            if !out.status.success() {
                return Err(failed(&self.tool(), &args, &out));
            }
            Ok(())
        } else {
            if current.is_none() {
                return Ok(());
            }
            // 从托管实例自己调用时，bootout 会结束本进程，下面不会执行（调用方只在 bind 之前这么做）
            self.bootout().await?;
            remove(&path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autostart::test_tool::FakeTool;

    fn agent(tool: &FakeTool, program: &str) -> LaunchAgent {
        LaunchAgent::new(AutostartConfig {
            program: PathBuf::from(program),
            home: tool.home().join("home"),
            tool: Some(tool.path()),
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

    /// bootstrap 失败 → Failed，带命令与输出；注册项写下了，下次对齐会再试。
    #[tokio::test]
    async fn macos_bootstrap_failure_is_reported() {
        let t = FakeTool::new();
        t.fail_on("bootstrap");
        let e = agent(&t, "/x/iwork-daemon").set(true).await.unwrap_err();
        assert!(
            matches!(&e, AutostartError::Failed { message } if message.contains("bootstrap")),
            "{e}"
        );
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
