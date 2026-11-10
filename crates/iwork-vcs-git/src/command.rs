//! 调本机 `git` 做写操作（技术设计第 8 节：写操作调本机 git，沿用用户的 hooks、凭据助手与配置）。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

/// 怎么启动 `git`：程序路径加额外的环境变量。
///
/// 生产用 [`GitCommand::default`]（`PATH` 里的 `git`，不加任何变量），用户的全局配置、凭据助手照常生效。
/// 额外变量只给测试用：把 `HOME` 和全局配置指到临时目录，测试才不碰用户的 git 配置。
#[derive(Debug, Clone)]
pub struct GitCommand {
    program: PathBuf,
    envs: Vec<(OsString, OsString)>,
}

impl Default for GitCommand {
    fn default() -> Self {
        Self {
            program: PathBuf::from("git"),
            envs: Vec::new(),
        }
    }
}

impl GitCommand {
    /// 给每次调用加一个环境变量。
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.envs
            .push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    /// 在 `cwd` 下准备一条 git 命令；标准输入关掉，输出由调用方决定。
    ///
    /// `LC_ALL=C`：进度与身份缺失的判断要读 git 的英文输出，翻译过的输出解析不了。
    pub(crate) fn command(&self, cwd: &Path) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.current_dir(cwd)
            .env("LC_ALL", "C")
            .envs(self.envs.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        cmd
    }
}

/// 一次跑完的 git 命令的结果。
pub(crate) struct Finished {
    pub success: bool,
    pub stderr: String,
}

impl GitCommand {
    /// 跑一条命令并等它结束，收集标准错误。启动失败（找不到 git）返回 `Err(说明)`。
    pub(crate) async fn run(&self, cwd: &Path, args: &[&str]) -> Result<Finished, String> {
        let output = self
            .command(cwd)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("启动 git 失败：{e}"))?;
        Ok(Finished {
            success: output.status.success(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
