//! 测试夹具：临时目录里的 git 仓库。
//!
//! 所有 git 调用都隔离用户的配置：`HOME` 指到临时目录，`GIT_CONFIG_GLOBAL` 指到临时目录里的文件，
//! 不读系统配置。需要提交身份时只写进这份临时的“全局”配置或测试仓库自己的配置。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use iwork_ports::vcs::{CloneProgress, ProgressSink};
use iwork_vcs_git::{GitCommand, GitVcs};
use tempfile::TempDir;

pub struct Fixture {
    pub tmp: TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("iwork-vcs-git-")
            .tempdir()
            .expect("create temp dir");
        let f = Self { tmp };
        std::fs::create_dir_all(f.home()).expect("create home");
        std::fs::write(f.global_config(), "").expect("create global config");
        f
    }

    /// 临时目录的真实路径（macOS 上 /var 是 /private/var 的符号链接）。
    pub fn root(&self) -> PathBuf {
        dunce_canonical(self.tmp.path())
    }

    pub fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    fn global_config(&self) -> PathBuf {
        self.home().join(".gitconfig")
    }

    /// 在临时的“全局”配置里写入提交身份（不碰用户真实的配置）。
    pub fn set_identity(&self) {
        self.git(
            &self.root(),
            &["config", "--global", "user.name", "iWork Test"],
        );
        self.git(
            &self.root(),
            &["config", "--global", "user.email", "test@example.invalid"],
        );
    }

    fn isolated_envs(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("HOME", self.home()),
            ("GIT_CONFIG_GLOBAL", self.global_config()),
            ("GIT_CONFIG_NOSYSTEM", PathBuf::from("1")),
        ]
    }

    /// 被测的 `GitVcs`，git 子进程用隔离的配置。
    pub fn vcs(&self) -> GitVcs {
        GitVcs::with_command(self.command())
    }

    pub fn command(&self) -> GitCommand {
        self.isolated_envs()
            .into_iter()
            .fold(GitCommand::default(), |c, (k, v)| c.env(k, v))
    }

    /// 搭夹具用：在 `cwd` 跑 git，失败就让测试挂掉。
    pub fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(cwd)
            .envs(self.isolated_envs())
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} 失败：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// 建一个有一次提交的仓库（提交身份写在仓库自己的配置里）。
    pub fn repo_with_commit(&self, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = self.root().join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        self.git(&dir, &["init", "--quiet"]);
        self.git(&dir, &["config", "user.name", "iWork Test"]);
        self.git(&dir, &["config", "user.email", "test@example.invalid"]);
        for (path, content) in files {
            write(&dir.join(path), content);
        }
        self.git(&dir, &["add", "--all"]);
        self.git(&dir, &["commit", "--quiet", "--allow-empty", "-m", "c1"]);
        dir
    }

    pub fn commit_all(&self, dir: &Path, message: &str) {
        self.git(dir, &["add", "--all"]);
        self.git(dir, &["commit", "--quiet", "-m", message]);
    }
}

pub fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir parent");
    }
    std::fs::write(path, content).expect("write file");
}

pub fn dunce_canonical(p: &Path) -> PathBuf {
    // 与实现（detect）同一种规范化：Windows 上去掉 `\\?\` 前缀
    dunce::canonicalize(p).expect("canonicalize")
}

/// 记下所有进度的接收方。
#[derive(Clone, Default)]
pub struct Recorder(pub Arc<Mutex<Vec<CloneProgress>>>);

impl ProgressSink for Recorder {
    fn progress(&self, progress: CloneProgress) {
        self.0.lock().expect("lock").push(progress);
    }
}

impl Recorder {
    pub fn percents(&self) -> Vec<u8> {
        self.0
            .lock()
            .expect("lock")
            .iter()
            .map(|p| p.percent)
            .collect()
    }
}
