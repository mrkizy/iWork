//! 会话目录 `<数据目录>/run/h/<12 位十六进制>/`（T06 方案 3.3）：里面只有 socket、设置文件、MCP 配置、属主文件，
//! 和自动记忆改道用的空目录 `m/`（3.1a）。
//!
//! - 一定放在数据目录里：设置文件会被 CLI 热加载，Agent 要是改得了它就能关掉 hook；数据目录是系统底线。
//! - 名字写得短：Unix socket 路径有长度上限（macOS 103 字节，Linux 107）。建之前先算，超了报错点名路径。
//! - 随机数只让各会话的目录不重名（`mkdir` 独占，重名换一个再试）；通道的安全靠 0700、对端校验和令牌。
//! - 正常退出删掉自己的目录；被 `SIGKILL` 留下的，下一个桥接启动时按属主进程号清扫。
//!
//! M1 不支持 Windows（AGENTS.md）：那边的命名管道随 S6 接，这里只有 unix 的实现。

use std::fmt;
use std::path::{Path, PathBuf};

/// 会话目录里的四个文件名。
pub const SOCKET: &str = "s";
pub const SETTINGS: &str = "settings.json";
pub const MCP: &str = "mcp.json";
pub const OWNER: &str = "owner";
/// 自动记忆改道的目录（3.1a）：桥接建好之后不往里放东西；CLI 万一写了，删会话目录时连同它一起删。
pub const MEMORY: &str = "m";

/// 随机部分的字节数（十六进制后 12 个字符）。
#[cfg(unix)]
const RANDOM_BYTES: usize = 6;

/// 重名时最多换几次名字。
#[cfg(unix)]
const CREATE_ATTEMPTS: usize = 8;

/// Unix socket 路径的上限（字节，不含结尾的 NUL）。
#[cfg(target_os = "macos")]
pub const SOCKET_PATH_MAX: usize = 103;
#[cfg(not(target_os = "macos"))]
pub const SOCKET_PATH_MAX: usize = 107;

/// 会话目录里各文件的路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Files {
    pub socket: PathBuf,
    pub settings: PathBuf,
    pub mcp: PathBuf,
    pub owner: PathBuf,
    /// 自动记忆改道的空目录
    pub memory: PathBuf,
}

impl Files {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            socket: dir.join(SOCKET),
            settings: dir.join(SETTINGS),
            mcp: dir.join(MCP),
            owner: dir.join(OWNER),
            memory: dir.join(MEMORY),
        }
    }
}

/// 会话目录建不起来、不安全、路径太长：开会话失败 `channel_failed`。说明里点名路径。
#[derive(Debug)]
pub struct DirError(pub String);

impl fmt::Display for DirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DirError {}

/// 所有会话目录的上一层：`<数据目录>/run/h`。
pub fn root(data_dir: &Path) -> PathBuf {
    data_dir.join("run").join("h")
}

/// socket 路径会不会超过上限。
pub fn check_socket_len(socket: &Path) -> Result<(), DirError> {
    let len = socket.as_os_str().len();
    if len > SOCKET_PATH_MAX {
        return Err(DirError(format!(
            "审批通道的 socket 路径太长（{len} 字节，上限 {SOCKET_PATH_MAX}）：{}",
            socket.display()
        )));
    }
    Ok(())
}

/// 一个建好的会话目录，自己负责删：显式 [`SessionDir::remove`] 拿得到删除的结果；没调过就在离开作用域时删
/// （开会话的 future 被丢掉、会话任务被丢掉、起任务失败，都只删这一次，审查 S6-3 1/F-4）。删不掉只记日志。
/// 进程被杀时留下的由下一个桥接启动时的 [`sweep`] 清。
#[derive(Debug)]
pub struct SessionDir {
    path: PathBuf,
    /// 还归它删：`remove` 之后为假
    #[cfg_attr(
        not(unix),
        allow(dead_code, reason = "建与删只在 unix 上实现（M1 不支持 Windows）")
    )]
    owned: bool,
}

impl SessionDir {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn files(&self) -> Files {
        Files::in_dir(&self.path)
    }
}

#[cfg(unix)]
mod imp {
    use std::fs::{self, DirBuilder, OpenOptions};
    use std::io::{self, Write};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};

    use iwork_claude_hook::peer::current_user;

    use super::*;

    /// 是自己的、真的目录（不是符号链接），权限没有给组与别人。
    fn check_private_dir(path: &Path) -> Result<(), DirError> {
        let bad = |why: &str| DirError(format!("{why}：{}", path.display()));
        let meta =
            fs::symlink_metadata(path).map_err(|e| bad(&format!("读不了目录信息（{e}）")))?;
        if !meta.file_type().is_dir() {
            return Err(bad("不是目录（可能是符号链接）"));
        }
        if meta.uid() != current_user() {
            return Err(bad("属主不是当前用户"));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(bad("权限给了组或别人"));
        }
        Ok(())
    }

    /// 数据目录下到会话目录上一层的路径：`run`、`run/h`，逐级检查，不跟链接（S5 审查 1/F-2）。
    /// 创建与清扫共用这一处：哪一级是符号链接、不是目录、不是自己的，都在碰它下面任何东西之前就停下。
    /// `create`：缺的那一级用 0700 新建（一次只建一级，建完再查）；不建时缺一级就返回 `None`（什么都没有可清）。
    /// `run` 只要求是自己的真目录（后台的本机 socket 也在这一层）；`run/h` 还要求是私有的（0700）。
    fn session_root(data_dir: &Path, create: bool) -> Result<Option<PathBuf>, DirError> {
        let mut path = data_dir.to_path_buf();
        for (level, private) in [("run", false), ("h", true)] {
            path.push(level);
            match fs::symlink_metadata(&path) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                    match DirBuilder::new().mode(0o700).create(&path) {
                        Ok(()) => {}
                        // 别的进程刚建好：照样往下查
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => {
                            return Err(DirError(format!("建不了 {}（{e}）", path.display())));
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => {
                    return Err(DirError(format!(
                        "读不了目录信息（{e}）：{}",
                        path.display()
                    )));
                }
            }
            if private {
                check_private_dir(&path)?;
            } else {
                check_own_dir(&path)?;
            }
        }
        Ok(Some(path))
    }

    /// 是自己的真目录（不是符号链接）。
    fn check_own_dir(path: &Path) -> Result<(), DirError> {
        let bad = |why: &str| DirError(format!("{why}：{}", path.display()));
        let meta =
            fs::symlink_metadata(path).map_err(|e| bad(&format!("读不了目录信息（{e}）")))?;
        if !meta.file_type().is_dir() {
            return Err(bad("不是目录（可能是符号链接）"));
        }
        if meta.uid() != current_user() {
            return Err(bad("属主不是当前用户"));
        }
        Ok(())
    }

    fn random_name() -> Result<String, DirError> {
        let mut bytes = [0u8; RANDOM_BYTES];
        getrandom::fill(&mut bytes).map_err(|e| DirError(format!("取随机数失败：{e}")))?;
        Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// 只写一次的 0600 文件；已存在就失败（不覆盖别人放的东西）。
    fn write_new(path: &Path, bytes: &[u8]) -> Result<(), DirError> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| DirError(format!("写不了 {}（{e}）", path.display())))?;
        f.write_all(bytes)
            .and_then(|()| f.sync_all())
            .map_err(|e| DirError(format!("写不了 {}（{e}）", path.display())))
    }

    impl SessionDir {
        /// 新建一个会话目录并写下属主（本进程号）。`run`、`run/h` 不存在就逐级建（0700），存在要过 `session_root` 的检查。
        pub fn create(data_dir: &Path) -> Result<Self, DirError> {
            let root = session_root(data_dir, true)?.expect("create 时缺的那一级已经建好");
            for _ in 0..CREATE_ATTEMPTS {
                let path = root.join(random_name()?);
                check_socket_len(&path.join(SOCKET))?;
                match DirBuilder::new().mode(0o700).create(&path) {
                    Ok(()) => {}
                    // 重名：换一个，不复用、不删别人的
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(DirError(format!("建不了 {}（{e}）", path.display()))),
                }
                check_private_dir(&path)?;
                let dir = Self { path, owned: true };
                write_new(
                    &dir.files().owner,
                    std::process::id().to_string().as_bytes(),
                )?;
                let memory = dir.files().memory;
                DirBuilder::new()
                    .mode(0o700)
                    .create(&memory)
                    .map_err(|e| DirError(format!("建不了 {}（{e}）", memory.display())))?;
                return Ok(dir);
            }
            Err(DirError(format!(
                "{} 下连续 {CREATE_ATTEMPTS} 次重名",
                root.display()
            )))
        }

        /// 写设置文件与 MCP 配置（0600，只写一次）。
        pub fn write_config(&self, settings: &[u8], mcp: &[u8]) -> Result<(), DirError> {
            let files = self.files();
            write_new(&files.settings, settings)?;
            write_new(&files.mcp, mcp)
        }

        /// 正常退出：删掉自己的会话目录。不管成没成，之后离开作用域时都不再删。
        pub fn remove(mut self) -> io::Result<()> {
            self.owned = false;
            remove_known(&self.path)
        }
    }

    impl Drop for SessionDir {
        fn drop(&mut self) {
            if self.owned
                && let Err(e) = remove_known(&self.path)
            {
                tracing::warn!(error = %e, path = %self.path.display(), "删会话目录失败");
            }
        }
    }

    /// 只删会话目录里那几个名字，再删目录本身；目录里有别的东西就留着不删（不是自己建的那种）。
    /// `m/` 是真目录（不是链接）才删，连同 CLI 可能写进去的内容（`remove_dir_all` 不跟里面的链接走）。
    fn remove_known(dir: &Path) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let name = entry?.file_name();
            if ![SOCKET, SETTINGS, MCP, OWNER, MEMORY]
                .iter()
                .any(|n| name == *n)
            {
                return Err(io::Error::other(format!(
                    "{} 里有不认识的文件 {name:?}，不删",
                    dir.display()
                )));
            }
        }
        let memory = dir.join(MEMORY);
        match fs::symlink_metadata(&memory) {
            Ok(m) if m.file_type().is_dir() => fs::remove_dir_all(&memory)?,
            Ok(_) => {
                return Err(io::Error::other(format!(
                    "{} 不是目录（可能是链接），不删",
                    memory.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        for name in [SOCKET, SETTINGS, MCP, OWNER] {
            match fs::remove_file(dir.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        fs::remove_dir(dir)
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: kill 的信号 0 只检查进程在不在、有没有权限，不发信号
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// 启动时清扫：删掉 `run/h/` 下属主进程已不在的会话目录，返回删了几个。只认「12 位十六进制名字、
    /// 自己的私有目录、属主文件能读出进程号、里面只有那四个名字」的目录；别的一律不碰。
    /// 进程号被复用时最多晚删一轮。
    pub fn sweep(data_dir: &Path) -> io::Result<usize> {
        // 先过同一个边界检查：run 或 run/h 是链接、不是自己的，就不读、不删，报错
        let Some(root) = session_root(data_dir, false).map_err(|e| io::Error::other(e.0))? else {
            return Ok(0);
        };
        let entries = fs::read_dir(&root)?;
        let mut removed = 0;
        for entry in entries {
            let path: PathBuf = entry?.path();
            let ours = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.len() == RANDOM_BYTES * 2 && n.bytes().all(|b| b.is_ascii_hexdigit())
            });
            if !ours || check_private_dir(&path).is_err() {
                continue;
            }
            let Some(pid) = fs::read_to_string(path.join(OWNER))
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .filter(|p| *p > 0)
            else {
                continue;
            };
            if alive(pid) {
                continue;
            }
            if remove_known(&path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(unix)]
pub use imp::sweep;

#[cfg(test)]
#[cfg(unix)]
#[path = "session_dir_tests.rs"]
mod tests;
