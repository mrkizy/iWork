//! 自行重启（方案 2.9；S7、S7a、S7b–d、S7f、S16）。后台带 `IWORK_DEV_INSTALL_FAST=10`：安装检查的全部时长除以 10
//! （周期 3s、冷却 3s、「安装中」的重查 0.2/0.4/0.8/1.6s）；真实时长由 `tasks::install_watch` 的单测用暂停的时钟量。
//!
//! 换版本一律先写临时文件再改名进去：覆盖正在运行的程序文件，Linux 报 ETXTBSY，macOS 会因代码签名失效杀掉进程。
//! B 版的造法：macOS 用另一个签名标识做 ad-hoc 重签（cdhash 变），Linux 在文件末尾追加字节（全文哈希变）。
//! 「重启成了哪一版」看日志里每次「后台服务就绪」带的运行映像身份。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use iwork_ipc::peer::{self, ImageId, MANIFEST_FILE, Want};

use crate::support::{DAEMON, Daemon, Sandbox, append_exe, code, copy_exe, log_text};

const FAST: (&str, &str) = ("IWORK_DEV_INSTALL_FAST", "10");
/// 每条用例要复制、重签几个 20 MB 的程序并反复算哈希；本机负载高（其他构建、杀毒软件扫新程序）时这些都慢，
/// 所以「等它发生」的窗口放得宽（判定周期 3s + 冷却 3s 之外全是余量），同时最多 2 条安装用例一起跑。
const START: Duration = Duration::from_secs(60);
const RESTART: Duration = Duration::from_secs(60);

/// 安装用例的并发上限。
static SLOTS: (std::sync::Mutex<usize>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());
const MAX_SLOTS: usize = 2;

pub struct Slot;

impl Slot {
    pub fn take() -> Slot {
        let mut n = SLOTS.0.lock().unwrap();
        while *n >= MAX_SLOTS {
            n = SLOTS.1.wait(n).unwrap();
        }
        *n += 1;
        Slot
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        *SLOTS.0.lock().unwrap() -= 1;
        SLOTS.1.notify_one();
    }
}

/// 等日志里出现 `needle`。
fn wait_log(sb: &Sandbox, needle: &str, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !log_text(sb).contains(needle) {
        assert!(
            Instant::now() < deadline,
            "{limit:?} 内日志没出现「{needle}」"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 一个安装目录。
pub struct Install {
    pub dir: PathBuf,
}

impl Install {
    pub fn new(sb: &Sandbox, rel: &str) -> Install {
        let dir = sb.home.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        Install { dir }
    }

    pub fn daemon(&self) -> PathBuf {
        self.dir.join("iwork-daemon")
    }

    /// 造一个 `tag` 版本的二进制（后台程序本身的拷贝），放到 `staged` 路径（不在安装目录里）。
    pub fn build(&self, name: &str, tag: &str) -> PathBuf {
        let staged = self.dir.parent().unwrap().join(format!(".{name}.{tag}"));
        if staged.exists() {
            return staged;
        }
        let tmp = staged.with_extension("tmp");
        copy_exe(Path::new(DAEMON), &tmp);
        if cfg!(target_os = "macos") {
            let ok = std::process::Command::new("codesign")
                .args(["-f", "-s", "-", "-i", &format!("dev.iwork.{name}.{tag}")])
                .arg(&tmp)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "codesign 失败");
        } else {
            append_exe(&tmp, &format!("iwork-test-variant-{tag}"));
        }
        std::fs::rename(&tmp, &staged).unwrap();
        staged
    }

    /// 把 `tag` 版的 `name` 放进安装目录（改名进去，原子）。
    pub fn put(&self, name: &str, tag: &str) {
        let src = self.build(name, tag);
        let tmp = self.dir.join(format!(".{name}.incoming"));
        copy_exe(&src, &tmp);
        std::fs::rename(&tmp, self.dir.join(name)).unwrap();
    }

    /// 放半个 `tag` 版的 `name`（写到一半的样子）。
    pub fn put_half(&self, name: &str, tag: &str) {
        let src = self.build(name, tag);
        let bytes = std::fs::read(&src).unwrap();
        let tmp = self.dir.join(format!(".{name}.incoming"));
        std::fs::write(&tmp, &bytes[..bytes.len() / 2]).unwrap();
        std::fs::rename(&tmp, self.dir.join(name)).unwrap();
    }

    /// 清单文本：登记这些 (二进制名, 版本) 的全部身份。
    pub fn manifest_text(&self, entries: &[(&str, &str)]) -> String {
        let body: Vec<String> = entries
            .iter()
            .map(|(name, tag)| {
                let ids = peer::file_image_ids(&self.build(name, tag), Want::All).unwrap();
                let list: Vec<String> = ids.iter().map(|i| format!("\"{i}\"")).collect();
                format!("\"{name}\":[{}]", list.join(","))
            })
            .collect();
        format!(
            r#"{{"manifest_version":1,"app_version":"t","binaries":{{{}}}}}"#,
            body.join(",")
        )
    }

    pub fn put_manifest(&self, entries: &[(&str, &str)]) {
        let tmp = self.dir.join(".manifest.incoming");
        std::fs::write(&tmp, self.manifest_text(entries)).unwrap();
        std::fs::rename(&tmp, self.dir.join(MANIFEST_FILE)).unwrap();
    }

    /// 装齐 `tag` 版：后台、客户端、清单。
    pub fn put_all(&self, tag: &str) {
        self.put("iwork-daemon", tag);
        self.put("iwork-client", tag);
        self.put_manifest(&[("iwork-daemon", tag), ("iwork-client", tag)]);
    }

    /// `tag` 版后台运行起来时的映像身份（和运行身份同一种）。
    pub fn image(&self, tag: &str) -> String {
        image_of(&self.build("iwork-daemon", tag))
    }
}

/// 一个程序文件「运行起来」时的身份：macOS 是 cdhash，Linux 是全文 SHA-256（N5：静态 = 运行中）。
pub fn image_of(path: &Path) -> String {
    let ids = peer::file_image_ids(path, Want::All).unwrap();
    let pick = ids
        .iter()
        .find(|i| {
            if cfg!(target_os = "macos") {
                matches!(i, ImageId::CdHash(_))
            } else {
                matches!(i, ImageId::Sha256(_))
            }
        })
        .unwrap();
    pick.to_string()
}

/// 日志里每次「后台服务就绪」的运行映像身份，按先后。
pub fn readies(sb: &Sandbox) -> Vec<String> {
    log_text(sb)
        .lines()
        .filter(|l| l.contains("后台服务就绪"))
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            v["fields"]["image"].as_str().map(str::to_owned)
        })
        .collect()
}

pub fn wait_readies(sb: &Sandbox, n: usize, limit: Duration) -> Vec<String> {
    let deadline = Instant::now() + limit;
    loop {
        let r = readies(sb);
        if r.len() >= n {
            return r;
        }
        assert!(
            Instant::now() < deadline,
            "{limit:?} 内没等到第 {n} 次就绪：{r:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 起安装目录里的后台（加快安装检查的计时），等它就绪。
pub fn start(sb: &Sandbox, inst: &Install, args: &[&str]) -> Daemon {
    let d = sb.spawn_exe(&inst.daemon(), args, &[FAST]);
    sb.wait_ready(d.pid(), START);
    d
}

fn installed(sb: &Sandbox, tag: &str) -> Install {
    let inst = Install::new(sb, "app");
    inst.put_all(tag);
    inst
}

/// 断言在 `quiet` 期间没有重启（就绪次数不变、进程还在）。
fn assert_no_restart(sb: &Sandbox, d: &mut Daemon, quiet: Duration, expected: usize) {
    std::thread::sleep(quiet);
    assert_eq!(readies(sb).len(), expected, "{quiet:?} 内不该重启");
    assert!(d.try_exit().is_none(), "进程还在");
}

/// 装 A 起 A → 一次装齐 B → 重启成 B（同一个 pid，exec 过），之后不再重启。
#[test]
fn replaced_install_triggers_exec() {
    let _slot = Slot::take();
    let sb = Sandbox::new();
    let inst = installed(&sb, "A");
    let mut d = start(&sb, &inst, &[]);
    assert_eq!(readies(&sb), [inst.image("A")]);
    inst.put_all("B");
    let r = wait_readies(&sb, 2, RESTART);
    assert_eq!(r[1], inst.image("B"), "重启成了 B");
    sb.wait_ready(d.pid(), START);
    // S7：升级之后不循环重启
    assert_no_restart(&sb, &mut d, Duration::from_secs(8), 2);
}

/// S7：新后台写到一半、停一会儿、再写完、最后换清单：停顿期间不动，装齐后才重启成 B。
#[test]
fn slow_install_does_not_restart_until_complete() {
    let _slot = Slot::take();
    let sb = Sandbox::new();
    let inst = installed(&sb, "A");
    let mut d = start(&sb, &inst, &[]);
    inst.put_half("iwork-daemon", "B");
    wait_log(&sb, "安装中", RESTART);
    assert_no_restart(&sb, &mut d, Duration::from_secs(5), 1);
    inst.put("iwork-daemon", "B");
    assert_no_restart(&sb, &mut d, Duration::from_secs(4), 1);
    inst.put("iwork-client", "B");
    inst.put_manifest(&[("iwork-daemon", "B"), ("iwork-client", "B")]);
    let r = wait_readies(&sb, 2, RESTART);
    assert_eq!(r[1], inst.image("B"));
}

/// S7：三种到位顺序，最后一个文件到位之前都不动，之后重启成 B。
#[test]
fn staged_install_any_order() {
    type Step = fn(&Install);
    let daemon: Step = |i| i.put("iwork-daemon", "B");
    let client: Step = |i| i.put("iwork-client", "B");
    let manifest: Step = |i| i.put_manifest(&[("iwork-daemon", "B"), ("iwork-client", "B")]);
    for order in [
        [daemon, client, manifest],
        [manifest, daemon, client],
        [client, manifest, daemon],
    ] {
        let _slot = Slot::take();
        let sb = Sandbox::new();
        let inst = installed(&sb, "A");
        let mut d = start(&sb, &inst, &[]);
        for step in &order[..2] {
            step(&inst);
            assert_no_restart(&sb, &mut d, Duration::from_secs(2), 1);
        }
        order[2](&inst);
        let r = wait_readies(&sb, 2, RESTART);
        assert_eq!(r[1], inst.image("B"));
    }
}

/// S7（第 1 轮 F-5）：无清单启动 → 之后放入清单（文件没变）→ 重启一次，把清单读进来，之后不再重启。
/// 「读进来了」看行为：没读进来的话，内存里没有清单、磁盘上有，每个周期都会判「信任材料换了」而循环重启。
/// （`manifest_loaded` 审计记在第 1 步，那时文件日志还没装，不在 daemon.log 里。）
#[test]
fn manifest_arrives_after_start() {
    let _slot = Slot::take();
    let sb = Sandbox::new();
    let inst = Install::new(&sb, "app");
    inst.put("iwork-daemon", "A");
    let mut d = start(&sb, &inst, &[]);
    inst.put_manifest(&[("iwork-daemon", "A")]);
    let r = wait_readies(&sb, 2, RESTART);
    assert_eq!(r[0], r[1], "同一个程序文件");
    sb.wait_ready(d.pid(), START);
    assert_no_restart(&sb, &mut d, Duration::from_secs(10), 2);
}

/// S7：装齐的新后台没有执行权限 → `exec` 失败，以 21 退出（托管实例同样报非零）。
#[test]
fn exec_failure_exits_restart_requested() {
    use std::os::unix::fs::PermissionsExt;
    let _slot = Slot::take();
    let sb = Sandbox::new();
    let inst = installed(&sb, "A");
    let mut d = start(&sb, &inst, &["--service"]);
    let b = inst.build("iwork-daemon", "B");
    let tmp = inst.dir.join(".noexec");
    copy_exe(&b, &tmp);
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::rename(&tmp, inst.daemon()).unwrap();
    inst.put("iwork-client", "B");
    inst.put_manifest(&[("iwork-daemon", "B"), ("iwork-client", "B")]);
    assert_eq!(
        code(d.wait_exit(RESTART + START)),
        Some(21),
        "{}",
        d.stderr()
    );
    assert!(d.stderr().contains("exec 失败"));
}

/// S7a：只换了清单（B）、程序文件还是 A；这时 A 崩了、再起（磁盘上的程序还是 A）→ 不重启；
/// 之后补齐 B 的二进制 → 重启成 B。参照物是运行映像，不是启动时的磁盘。
#[test]
fn old_daemon_restarted_mid_install_still_upgrades() {
    let _slot = Slot::take();
    let sb = Sandbox::new();
    let inst = installed(&sb, "A");
    // B 的二进制先造好（清单要登记它们的身份），但不放进去
    inst.build("iwork-daemon", "B");
    inst.build("iwork-client", "B");
    let mut a = start(&sb, &inst, &[]);
    inst.put_manifest(&[("iwork-daemon", "B"), ("iwork-client", "B")]);
    a.kill9();
    let mut a2 = start(&sb, &inst, &[]);
    assert_no_restart(&sb, &mut a2, Duration::from_secs(8), 2);
    inst.put("iwork-daemon", "B");
    inst.put("iwork-client", "B");
    let r = wait_readies(&sb, 3, RESTART);
    assert_eq!(r[2], inst.image("B"));
}

/// 同上的变体：接班的是带 `--service` 的等锁实例，且二进制先到、清单后到。
#[test]
fn old_daemon_restarted_mid_install_still_upgrades_via_standby() {
    let _slot = Slot::take();
    let sb = Sandbox::new();
    let inst = installed(&sb, "A");
    let mut a = start(&sb, &inst, &[]);
    let standby = sb.spawn_exe(&inst.daemon(), &["--service"], &[FAST]);
    standby.wait_stderr("等锁接班", START);
    inst.put("iwork-daemon", "B");
    // 磁盘上已是 B 的后台、清单还是 A：A 崩了，等锁的实例 exec 磁盘上的程序（B）接班
    a.kill9();
    sb.wait_ready(standby.pid(), START);
    let r = readies(&sb);
    assert_eq!(r.last().unwrap(), &inst.image("B"), "接班的是磁盘上的 B");
    let before = r.len();
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(
        readies(&sb).len(),
        before,
        "清单还是 A：B 的后台对不上，装着没齐，不动"
    );
    inst.put("iwork-client", "B");
    inst.put_manifest(&[("iwork-daemon", "B"), ("iwork-client", "B")]);
    // 清单换了（信任材料变了）→ 重启一次把清单读进来
    let r = wait_readies(&sb, before + 1, RESTART);
    assert_eq!(r.last().unwrap(), &inst.image("B"));
}

/// S16、S7a：后台刚起来时磁盘上已经装齐了另一版 → 第一次判定就是 Restart，但冷却到点才重启（不早于启动后 3s）。
#[test]
fn start_after_complete_install_of_other_version_restarts_after_cooldown() {
    let _slot = Slot::take();
    let sb = Sandbox::new();
    let inst = installed(&sb, "A");
    let t = Instant::now();
    let d = sb.spawn_exe(&inst.daemon(), &[], &[FAST]);
    // 等它过了第 1 步（写出状态文件）就装齐 B。第 1 步取运行身份时路径上的文件被换掉，macOS 上会取不到、以 10 退出
    // （不写状态文件，客户端照常重拉），那不是这条用例要测的
    let deadline = Instant::now() + START;
    while sb.status().is_none_or(|s| s.pid != d.pid()) {
        assert!(Instant::now() < deadline, "没写出状态文件");
        std::thread::sleep(Duration::from_millis(5));
    }
    inst.put_all("B");
    sb.wait_ready(d.pid(), START);
    let r = wait_readies(&sb, 2, RESTART);
    assert_eq!(r[1], inst.image("B"));
    assert!(
        t.elapsed() >= Duration::from_secs(3),
        "冷却：{:?}",
        t.elapsed()
    );
}

/// S7f：安装开了个头（清单换成 B）又撤回（清单 A 放回去）→ 磁盘已和我一样，不重启；日志里有「离开安装中」。
#[test]
fn aborted_install_restored_does_not_restart() {
    let _slot = Slot::take();
    let sb = Sandbox::new();
    let inst = installed(&sb, "A");
    inst.build("iwork-daemon", "B");
    inst.build("iwork-client", "B");
    let mut d = start(&sb, &inst, &[]);
    let a_manifest = std::fs::read(inst.dir.join(MANIFEST_FILE)).unwrap();
    inst.put_manifest(&[("iwork-daemon", "B"), ("iwork-client", "B")]);
    wait_log(&sb, "安装中", RESTART);
    let tmp = inst.dir.join(".m");
    std::fs::write(&tmp, a_manifest).unwrap();
    std::fs::rename(&tmp, inst.dir.join(MANIFEST_FILE)).unwrap();
    assert_no_restart(&sb, &mut d, Duration::from_secs(9), 1);
    assert!(log_text(&sb).contains("离开「安装中」"));
    assert!(!log_text(&sb).contains("开始停机"));
}

/// S7：装到一半被丢下不管 → 一直「安装中」，过了 5 分钟（快进后 30s）记一条 warn，写明卡在哪。
#[test]
fn abandoned_install_warns_after_a_while() {
    let _slot = Slot::take();
    let sb = Sandbox::new();
    let inst = installed(&sb, "A");
    let mut d = start(&sb, &inst, &[]);
    inst.put_half("iwork-daemon", "B");
    wait_log(&sb, "安装中持续太久", Duration::from_secs(120));
    let log = log_text(&sb);
    let line = log.lines().find(|l| l.contains("安装中持续太久")).unwrap();
    assert!(line.contains("manifest"), "写明各形态卡在哪：{line}");
    assert!(d.try_exit().is_none());
    assert_eq!(readies(&sb).len(), 1);
}

/// 起安装目录里的后台，等它退出，返回退出状态与 stderr。
fn run_installed(
    sb: &Sandbox,
    inst: &Install,
    args: &[&str],
    env: &[(&str, &str)],
) -> (Option<i32>, String) {
    let mut d = sb.spawn_exe(&inst.daemon(), args, env);
    let status = d.wait_exit(START);
    (code(status), d.stderr())
}

const FAIL_IDENTITY: (&str, &str) = ("IWORK_DEV_FAIL_AT", "identity");

/// S11f（1/F-9）：安装进行中被拉起的托管实例第 1 步失败 → 逻辑码 14、退出状态 14（非零，服务管理器会再拉），
/// 不拿锁、不写状态。两格：包没装齐；第 1 步期间程序文件被改写（戳变了）。补齐之后再起（服务管理器的下一次拉起）→ 就绪。
#[test]
fn identity_failure_mid_install_is_relaunched() {
    let _slot = Slot::take();
    let sb = Sandbox::new();
    let inst = installed(&sb, "A");
    // 清单登记的是 B 的后台，磁盘上还是 A：没装齐
    inst.put_manifest(&[("iwork-daemon", "B"), ("iwork-client", "A")]);
    let (status, err) = run_installed(&sb, &inst, &["--service"], &[FAIL_IDENTITY]);
    assert_eq!(status, Some(14), "{err}");
    assert!(
        err.contains("逻辑退出码 14") && err.contains("包没装齐"),
        "{err}"
    );
    assert!(sb.untouched(), "不拿锁、不写状态");

    // 另一格：包是齐的，第 1 步期间程序文件被改写（测试的子进程改它的修改时间）
    inst.put_manifest(&[("iwork-daemon", "A"), ("iwork-client", "A")]);
    let resume = sb.home.join("resume");
    let mut d = sb.spawn_exe(
        &inst.daemon(),
        &["--service"],
        &[
            FAIL_IDENTITY,
            ("IWORK_DEV_PAUSE_AT", "identity"),
            ("IWORK_DEV_RESUME_FILE", resume.to_str().unwrap()),
        ],
    );
    d.wait_stderr("IWORK-PAUSED identity", START);
    let ok = std::process::Command::new("touch")
        .args(["-m", "-t", "202001010000"])
        .arg(inst.daemon())
        .status()
        .unwrap()
        .success();
    assert!(ok, "touch 失败");
    std::fs::write(&resume, b"").unwrap();
    let status = d.wait_exit(START);
    assert_eq!(code(status), Some(14), "{}", d.stderr());
    assert!(
        d.stderr().contains("程序文件在启动期间变了"),
        "{}",
        d.stderr()
    );
    assert!(sb.untouched());

    // 装齐、去掉注入：下一次拉起到就绪
    start(&sb, &inst, &["--service"]);
}

/// S11f 对照（1/F-9）：包是装齐的、程序文件不动，第 1 步照样失败 → 逻辑码 10：托管实例退出状态 0（服务管理器不再拉），
/// 非托管 10；不拿锁、不写状态。
#[test]
fn identity_failure_on_settled_install_exits_zero() {
    let _slot = Slot::take();
    let sb = Sandbox::new();
    let inst = installed(&sb, "A");
    let (status, err) = run_installed(&sb, &inst, &["--service"], &[FAIL_IDENTITY]);
    assert_eq!(status, Some(0), "{err}");
    assert!(
        err.contains("逻辑退出码 10") && err.contains("稳定、装齐"),
        "{err}"
    );
    let (status, err) = run_installed(&sb, &inst, &[], &[FAIL_IDENTITY]);
    assert_eq!(status, Some(10), "{err}");
    assert!(sb.untouched());
}

#[cfg(target_os = "macos")]
mod bundle {
    //! 两种包形态之间的升级（S7b、S7c、S7d、S7e）。整包签名用 ad-hoc 整包签名造封印：对安装检查来说和证书签名
    //! 走同一条路（cdhash 比对 + 整包验签）；证书那一支在 `install::same_as_running` 的表里。

    use super::*;
    use std::process::Command;

    fn run(cmd: &mut Command) {
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{cmd:?}：{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// 一个 `iWork.app`。`sealed` 时整包签名、不放清单；否则不封印、放清单。
    fn make_app(sb: &Sandbox, root: &str, tag: &str, sealed: bool) -> PathBuf {
        let base = sb.home.join(root);
        let app = base.join("iWork.app");
        let macos = app.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::create_dir_all(app.join("Contents/Resources")).unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>dev.iwork.test.app</string>
  <key>CFBundleExecutable</key><string>iwork-client</string>
  <key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
"#,
        )
        .unwrap();
        std::fs::write(
            app.join("Contents/Resources/res.txt"),
            format!("资源 {tag}"),
        )
        .unwrap();
        let inst = Install { dir: macos.clone() };
        inst.put("iwork-daemon", tag);
        inst.put("iwork-client", tag);
        if sealed {
            run(Command::new("codesign").args(["-f", "-s", "-"]).arg(&app));
        } else {
            inst.put_manifest(&[("iwork-daemon", tag), ("iwork-client", tag)]);
        }
        app
    }

    /// 整包替换：把 `new` 改名到 `installed`（旧的先挪走）。
    fn swap(installed: &Path, new: &Path) {
        let old = installed.with_extension("old");
        std::fs::rename(installed, &old).unwrap();
        std::fs::rename(new, installed).unwrap();
    }

    fn daemon_in(app: &Path) -> PathBuf {
        app.join("Contents/MacOS/iwork-daemon")
    }

    fn start_app(sb: &Sandbox, app: &Path) -> Daemon {
        let d = sb.spawn_exe(&daemon_in(app), &[], &[FAST]);
        sb.wait_ready(d.pid(), START);
        d
    }

    /// S7d：带清单的 A → 整包签名的 B（整包替换）→ 重启成 B。
    #[test]
    fn manifest_package_to_signed_bundle() {
        let _slot = Slot::take();
        let sb = Sandbox::new();
        let a = make_app(&sb, "install", "A", false);
        let b = make_app(&sb, "incoming", "B", true);
        let b_image = image_of(&daemon_in(&b));
        let _d = start_app(&sb, &a);
        swap(&a, &b);
        let r = wait_readies(&sb, 2, RESTART);
        assert_eq!(r[1], b_image);
    }

    /// S7d：整包签名的 A → 带清单的 B（整包替换）→ 重启成 B。
    #[test]
    fn signed_bundle_to_manifest_package() {
        let _slot = Slot::take();
        let sb = Sandbox::new();
        let a = make_app(&sb, "install", "A", true);
        let b = make_app(&sb, "incoming", "B", false);
        let b_image = image_of(&daemon_in(&b));
        let _d = start_app(&sb, &a);
        swap(&a, &b);
        let r = wait_readies(&sb, 2, RESTART);
        assert_eq!(r[1], b_image);
    }

    /// S7c：整包签名的 B，后台与封印先到、一个资源文件最后到；全程没有客户端连接 → 资源补上后重启成 B。
    #[test]
    fn signed_bundle_resource_arrives_last() {
        let _slot = Slot::take();
        let sb = Sandbox::new();
        let a = make_app(&sb, "install", "A", true);
        let b = make_app(&sb, "incoming", "B", true);
        let b_image = image_of(&daemon_in(&b));
        let res = std::fs::read(b.join("Contents/Resources/res.txt")).unwrap();
        std::fs::remove_file(b.join("Contents/Resources/res.txt")).unwrap();
        let mut d = start_app(&sb, &a);
        swap(&a, &b);
        assert_no_restart(&sb, &mut d, Duration::from_secs(5), 1);
        std::fs::write(a.join("Contents/Resources/res.txt"), res).unwrap();
        let r = wait_readies(&sb, 2, RESTART);
        assert_eq!(r[1], b_image);
    }

    /// S7d / L8：把整包签名的 B 合并拷进旧目录、旧清单留着 → 多了一个没登记的文件，验签不过，一直「安装中」，
    /// 到点有 warn 写明两行各卡在哪；删掉旧清单 → 重启成 B。
    #[test]
    fn stale_manifest_in_signed_bundle_stays_installing() {
        let _slot = Slot::take();
        let sb = Sandbox::new();
        let a = make_app(&sb, "install", "A", false);
        let b = make_app(&sb, "incoming", "B", true);
        let b_image = image_of(&daemon_in(&b));
        let stale = std::fs::read(a.join("Contents/MacOS").join(MANIFEST_FILE)).unwrap();
        let mut d = start_app(&sb, &a);
        swap(&a, &b);
        std::fs::write(a.join("Contents/MacOS").join(MANIFEST_FILE), stale).unwrap();
        wait_log(&sb, "安装中持续太久", Duration::from_secs(120));
        let log = log_text(&sb);
        let line = log.lines().find(|l| l.contains("安装中持续太久")).unwrap();
        assert!(
            line.contains("manifest") && line.contains("signed-bundle"),
            "{line}"
        );
        assert_eq!(readies(&sb).len(), 1);
        assert!(d.try_exit().is_none());
        std::fs::remove_file(a.join("Contents/MacOS").join(MANIFEST_FILE)).unwrap();
        let r = wait_readies(&sb, 2, RESTART);
        assert_eq!(r[1], b_image);
    }
}
