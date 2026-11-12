//! 登录自启的实现（T04 方案 2.12；`AutostartPort`）。
//!
//! | 平台 | 做法 | 服务管理器的重启配置 |
//! | --- | --- | --- |
//! | macOS | `~/Library/LaunchAgents/com.iwork.daemon.plist` + `launchctl bootstrap` / `bootout`。plist 写 `Program`
//! |       | （包内后台程序的绝对路径；`BundleProgram` 用 bootstrap 加载一律失败，N1 第 10 项）。不用 `SMAppService`（N1） | `KeepAlive = { SuccessfulExit = false }`、`ThrottleInterval = 10` |
//! | Linux | `$XDG_CONFIG_HOME/systemd/user/iwork-daemon.service` + `systemctl --user` | `Restart=on-failure`、`RestartSec=10`；不加 `RestartForceExitStatus`（N4） |
//!
//! 两边表达的是同一条约定：非零退出（或被信号结束）就再拉，零就不拉；报零还是非零由后台自己决定（`--service`）。
//! 两份实现都只读写文件、调外部命令，所以在所有 Unix 上都编译（两边的测试本机都能跑）；`for_current_user` 按平台选。
//! 外部命令的路径可以注入（测试用记录参数的假命令替身）。

use std::path::{Path, PathBuf};

use iwork_ports::autostart::AutostartError;

pub mod linux;
pub mod macos;

/// 服务管理器起后台时带的参数（后台据此知道自己是托管实例，按 2.2 的角色表报退出状态）。
pub const SERVICE_ARG: &str = "--service";

/// 用哪个命令去改服务管理器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tool {
    /// 系统的 `launchctl` / `systemctl`：只有发布构建用。测试构建里调到它直接 panic（组 3 审查 2/F-4：测试只隔离了
    /// `HOME`，没隔离 launchd，真命令会动到本机用户真实的后台）。
    System,
    /// 替身（测试与开发构建）。
    Substitute(PathBuf),
    /// 什么都不做：`supported()` 报不支持，`state()` 报没注册，`set` 报错，从不运行任何命令。开发构建没给替身时的默认。
    Disabled,
}

/// 一份自启实现的配置。
#[derive(Debug, Clone)]
pub struct AutostartConfig {
    /// 后台程序的绝对路径（包内的 `iwork-daemon`）。
    pub program: PathBuf,
    /// 用户主目录。
    pub home: PathBuf,
    pub tool: Tool,
    /// 不检查 systemd 用户实例在不在（测试用）。
    pub assume_supported: bool,
}

impl AutostartConfig {
    /// 要运行的命令路径。`Disabled` 不运行任何东西。
    fn command(&self, system: &str) -> Result<PathBuf, AutostartError> {
        match &self.tool {
            Tool::System => {
                #[cfg(test)]
                panic!("测试构建不许运行系统的 {system}：注入替身（Tool::Substitute）");
                #[cfg(not(test))]
                Ok(PathBuf::from(system))
            }
            Tool::Substitute(p) => Ok(p.clone()),
            Tool::Disabled => Err(disabled()),
        }
    }

    fn disabled(&self) -> bool {
        self.tool == Tool::Disabled
    }
}

fn disabled() -> AutostartError {
    AutostartError::Unsupported {
        // 不写环境变量名：这段代码发布构建里也有，scripts/check-no-dev-trust.sh 不许发布产物里出现开发开关的名字
        reason: "开发构建没有给自启命令的替身，不动系统的服务管理器".into(),
    }
}

/// 按当前平台选实现。
#[cfg(target_os = "macos")]
pub type Platform = macos::LaunchAgent;
#[cfg(target_os = "linux")]
pub type Platform = linux::UserUnit;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub type Platform = Unsupported;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn for_current_user(cfg: AutostartConfig) -> Platform {
    Platform::new(cfg)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn for_current_user(_cfg: AutostartConfig) -> Platform {
    Unsupported
}

/// M1 不支持的平台（Windows，Q2 已决定）。
#[derive(Debug, Clone, Copy, Default)]
pub struct Unsupported;

impl iwork_ports::autostart::AutostartPort for Unsupported {
    async fn state(&self) -> Result<iwork_ports::autostart::AutostartState, AutostartError> {
        Ok(iwork_ports::autostart::AutostartState::NotRegistered)
    }
    fn supported(&self) -> Result<(), AutostartError> {
        Err(AutostartError::Unsupported {
            reason: "M1 暂不支持这个平台的登录自启".into(),
        })
    }
    async fn set(&self, _enabled: bool) -> Result<(), AutostartError> {
        Err(self.supported().unwrap_err())
    }
    async fn write_definition(&self) -> Result<(), AutostartError> {
        Err(self.supported().unwrap_err())
    }
}

/// 跑一个外部命令；非零退出 → `Failed`，带上命令与输出。
async fn run(tool: &Path, args: &[&str]) -> Result<std::process::Output, AutostartError> {
    let out = tokio::process::Command::new(tool)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| AutostartError::Failed {
            message: format!("运行 {} 失败：{e}", tool.display()),
        })?;
    Ok(out)
}

fn failed(tool: &Path, args: &[&str], out: &std::process::Output) -> AutostartError {
    AutostartError::Failed {
        message: format!(
            "{} {} 退出状态 {}：{}{}",
            tool.display(),
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
            String::from_utf8_lossy(&out.stdout).trim()
        ),
    }
}

/// 写定义文件，目录不存在就建。经 `iwork_rootfs::replace_file`：同目录独占新建临时文件（`O_CREAT|O_EXCL`，
/// 不跟随链接）写完再改名；目标本身是符号链接时拒绝。不会顺着链接覆写别的文件。
fn write_atomically(path: &Path, text: &str) -> Result<(), AutostartError> {
    let fail = |e: &dyn std::fmt::Display| AutostartError::Failed {
        message: format!("写 {} 失败：{e}", path.display()),
    };
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(fail(&"不是文件路径"));
    };
    std::fs::create_dir_all(dir).map_err(|e| fail(&e))?;
    iwork_rootfs::replace_file(dir, Path::new(name), text.as_bytes()).map_err(|e| fail(&e))
}

/// 读定义文件：不存在是 `None`。
fn read_definition(path: &Path) -> Result<Option<String>, AutostartError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AutostartError::Failed {
            message: format!("读 {} 失败：{e}", path.display()),
        }),
    }
}

/// 服务管理器那一半：有没有在管这个服务，管着的话实际载入的是哪个程序（取不到为 `None`）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Live {
    Absent,
    Loaded(Option<PathBuf>),
}

/// 两半合成一个状态：`definition` 是定义文件里认出的程序路径（文件不在为 `None`；在但认不出为空路径）。
/// 已注册 = 文件在、服务管理器在管、并且它实际载入的就是文件里这一份（组 3 审查 2/F-1：只看文件会把「写了新文件、
/// 旧的还载着」当成已对齐）。实际载入的取不到，按「不符」处理，宁可多做一次。
fn combine(definition: Option<PathBuf>, live: Live) -> iwork_ports::autostart::AutostartState {
    use iwork_ports::autostart::AutostartState;
    match (definition, live) {
        (None, Live::Absent) => AutostartState::NotRegistered,
        (Some(program), Live::Loaded(Some(loaded))) if loaded == program => {
            AutostartState::Registered { program }
        }
        _ => AutostartState::Partial,
    }
}

fn remove(path: &Path) -> Result<(), AutostartError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(AutostartError::Failed {
            message: format!("删 {} 失败：{e}", path.display()),
        }),
    }
}

#[cfg(test)]
pub(crate) mod test_tool {
    //! 假命令替身：记下每次改动类调用的参数；`fail_on` 文件里写的词出现在参数里时以 1 退出；用几个状态文件模拟
    //! 服务管理器那一半（实际载入的是哪份定义），查询类调用（`print` / `show`）按它们回答。

    use std::path::{Path, PathBuf};

    pub struct FakeTool {
        pub dir: tempfile::TempDir,
    }

    impl FakeTool {
        pub fn new() -> FakeTool {
            let dir = tempfile::tempdir().unwrap();
            let d = dir.path();
            let script = format!(
                r#"#!/bin/sh
# 查询类调用按状态文件回答、不记日志：
#   macOS：live = bootstrap 时那份 plist 的副本（服务管理器实际载入的定义）
#   Linux：enabled = 已启用；loaded = 上次 daemon-reload 时单元文件的副本
case " $* " in
  *" print "*)
    [ -f '{live}' ] || {{ echo "Could not find service" >&2; exit 113; }}
    echo "	program = $(sed -n 's#.*<key>Program</key><string>\(.*\)</string>.*#\1#p' '{live}')"
    exit 0 ;;
  *" show "*)
    if [ -f '{enabled}' ]; then echo UnitFileState=enabled; else echo UnitFileState=disabled; fi
    if [ -f '{unit}' ] && ! cmp -s '{unit}' '{loaded}'; then echo NeedDaemonReload=yes; else echo NeedDaemonReload=no; fi
    if [ -f '{loaded}' ]; then
      echo "ExecStart={{ path=$(sed -n 's#^ExecStart="\(.*\)" .*#\1#p' '{loaded}') ; argv[]=x ; }}"
    else echo "ExecStart="; fi
    exit 0 ;;
esac
echo "$@" >> '{log}'
if [ -f '{fail}' ]; then for a in "$@"; do grep -qx -- "$a" '{fail}' && exit 1; done; fi
case " $* " in
  *" bootstrap "*) cp "$3" '{live}' ;;
  *" bootout "*) rm -f '{live}' ;;
  *" daemon-reload "*) if [ -f '{unit}' ]; then cp '{unit}' '{loaded}'; else rm -f '{loaded}'; fi ;;
  *" enable "*) : > '{enabled}' ;;
  *" disable "*) rm -f '{enabled}' ;;
esac
exit 0
"#,
                log = d.join("calls.log").display(),
                fail = d.join("fail_on").display(),
                live = d.join("live").display(),
                enabled = d.join("enabled").display(),
                loaded = d.join("loaded").display(),
                unit = d.join("config/systemd/user/iwork-daemon.service").display(),
            );
            // 可执行文件交给子进程（cp）去写：本进程若开着它的写句柄，同一时刻别的测试线程 fork 出的子进程
            // 会继承这个句柄直到它 exec，期间执行这个文件在 Linux 上报 ETXTBSY（CI 实测）。
            let src = dir.path().join("tool.sh");
            std::fs::write(&src, script).unwrap();
            let path = dir.path().join("tool");
            let ok = std::process::Command::new("cp")
                .arg(&src)
                .arg(&path)
                .status()
                .unwrap()
                .success();
            assert!(ok, "cp 失败");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            FakeTool { dir }
        }

        pub fn path(&self) -> PathBuf {
            self.dir.path().join("tool")
        }

        pub fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("calls.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        pub fn fail_on(&self, word: &str) {
            std::fs::write(self.dir.path().join("fail_on"), format!("{word}\n")).unwrap();
        }

        pub fn stop_failing(&self) {
            std::fs::remove_file(self.dir.path().join("fail_on")).unwrap();
        }

        /// 服务管理器那一半：macOS 已载入（bootstrap 之后在，bootout 之后不在），Linux 已启用。
        pub fn live(&self) -> bool {
            self.dir.path().join("live").exists() || self.dir.path().join("enabled").exists()
        }

        /// 模拟用户或系统在 iWork 之外卸下（macOS）或禁用（Linux）了服务。
        pub fn drop_live(&self) {
            let _ = std::fs::remove_file(self.dir.path().join("live"));
            let _ = std::fs::remove_file(self.dir.path().join("enabled"));
        }

        pub fn home(&self) -> &Path {
            self.dir.path()
        }
    }
}
