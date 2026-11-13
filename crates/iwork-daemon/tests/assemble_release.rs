//! 装配脚本 `scripts/assemble-release.sh`（T04p 方案 4.2；R1–R6、R9、R10）。夹具是假的 `.app`（或 Linux 的程序），
//! 不跑 `tauri build`；核对用真的 `check-install` 示例程序。缺 bash、jq 时失败并说明，不跳过。
#![cfg(unix)]

mod package_support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use iwork_ipc::peer::{ImageId, MANIFEST_FILE, Manifest, Want, file_image_ids};
use package_support::*;

/// 装配在成品目录里的记录与锁（scripts/lib/owned-paths.sh 的 JOURNAL_NAME、assemble-release.sh 的 LOCK）。
const JOURNAL: &str = ".iwork-journal";
const LOCK: &str = ".iwork-assemble.lock";

/// 照装配脚本的写法往记录里追加一条：<名字>（相对成品目录）是装配的，身份是 <路径> 现在的身份。身份用脚本自己的
/// `_ident` 算（同一个真值，不在测试里另抄一份格式）。
fn journal_add(dist: &Path, name: &str, identity_of: &Path) {
    use std::io::Write;
    let lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/lib/owned-paths.sh");
    let out = Command::new(BASH)
        .arg("-c")
        .arg(r#"source "$1"; _ident "$2""#)
        .arg("sh")
        .arg(&lib)
        .arg(identity_of)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ident = String::from_utf8(out.stdout).unwrap();
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(dist.join(JOURNAL))
        .unwrap();
    writeln!(f, "add\t{name}\t{}", ident.trim()).unwrap();
}

/// 跑脚本用的 bash：macOS 上是系统自带的 3.2（打包命令实际用的就是它），别的平台用 PATH 里的。
const BASH: &str = if cfg!(target_os = "macos") {
    "/bin/bash"
} else {
    "bash"
};

/// `check-install` 示例程序。只跑这一个测试文件时 cargo 不编示例程序，这里用同一个 cargo、同一个 profile 补编一次。
fn check_install() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let profile_dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let path = profile_dir.join("examples/check-install");
        if !path.exists() {
            let mut cmd = Command::new(env!("CARGO"));
            cmd.args([
                "build",
                "-q",
                "-p",
                "iwork-daemon",
                "--example",
                "check-install",
            ])
            .arg("--manifest-path")
            .arg(repo_root().join("Cargo.toml"));
            if profile_dir.file_name().unwrap() == "release" {
                cmd.arg("--release");
            }
            run(&mut cmd);
        }
        assert!(path.exists(), "编完还是没有 {}", path.display());
        path
    })
}

/// 一次装配的输入：Tauri 的包（macOS）、包外的客户端与后台程序、成品目录。
struct Inputs {
    _t: tempfile::TempDir,
    root: PathBuf,
    app: PathBuf,
    client: PathBuf,
    daemon: PathBuf,
    dist: PathBuf,
}

impl Inputs {
    fn new(version: &str) -> Inputs {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let bins = root.join("bins");
        let client = place_program(&bins, CLIENT, version);
        let daemon = place_program(&bins, DAEMON, version);
        // Tauri 打出来的样子：只有客户端，没有封印（macOS）
        let app_root = root.join("tauri");
        let app = app_root.join("iWork.app");
        if cfg!(target_os = "macos") {
            let macos = app_skeleton(&app_root);
            std::fs::copy(&client, macos.join(CLIENT)).unwrap();
        }
        Inputs {
            root: root.clone(),
            app,
            client,
            daemon,
            dist: root.join("dist"),
            _t: t,
        }
    }

    fn command(&self) -> Command {
        // 用 macOS 自带的 bash 3.2、UTF-8 locale 跑：真打包就是这样跑的（审查 1/F-5）
        let mut cmd = Command::new(BASH);
        cmd.arg(repo_root().join("scripts/assemble-release.sh"))
            .env("LANG", "en_US.UTF-8")
            .env("LC_ALL", "en_US.UTF-8");
        if cfg!(target_os = "macos") {
            cmd.arg("--app").arg(&self.app);
        }
        cmd.arg("--client")
            .arg(&self.client)
            .arg("--daemon")
            .arg(&self.daemon)
            .arg("--check")
            .arg(check_install())
            .arg("--dist")
            .arg(&self.dist)
            .env_remove("IWORK_SIGN_IDENTITY")
            .env_remove("IWORK_SIGN_KEYCHAIN");
        cmd
    }

    fn assemble(&self) -> Output {
        self.command().output().unwrap()
    }

    /// 形态 A 的成品里放程序的目录。
    fn unsigned_bin(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.dist.join("unsigned/iWork.app/Contents/MacOS")
        } else {
            self.dist.join("linux/iwork")
        }
    }

    fn product(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.dist.join("unsigned/iWork.app")
        } else {
            self.dist.join("linux/iwork")
        }
    }
}

fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "退出 {:?}\nstdout：{}\nstderr：{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// 目录下每个文件的相对路径与全部内容，按路径排序。用来证明「一个字节都没动」。
fn digest_tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let bytes = std::fs::read(&p).unwrap();
                out.push((p.strip_prefix(dir).unwrap().to_path_buf(), bytes));
            }
        }
    }
    out.sort();
    out
}

/// 成品目录里除了装配自己的记录与锁之外的条目。
fn dist_entries(dist: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dist)
        .map(|rd| {
            rd.map(|e| e.unwrap().file_name().into_string().unwrap())
                .filter(|n| n != JOURNAL && n != LOCK)
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// R1：装配后的包里后台、客户端、清单都在同一个目录（macOS 的 `Contents/MacOS`）。
#[test]
fn assemble_puts_daemon_next_to_client() {
    let i = Inputs::new("a");
    ok(&i.assemble());
    let bin = i.unsigned_bin();
    for name in [DAEMON, CLIENT, MANIFEST_FILE] {
        assert!(bin.join(name).is_file(), "{} 不在 {}", name, bin.display());
    }
    assert_eq!(
        std::fs::read(bin.join(DAEMON)).unwrap(),
        std::fs::read(&i.daemon).unwrap(),
        "放进去的就是给的后台程序"
    );
    assert_eq!(
        dist_entries(&i.dist),
        [if cfg!(target_os = "macos") {
            "unsigned"
        } else {
            "linux"
        }],
        "发布完没有留下暂存或留底"
    );
}

/// R1 `binary_names_agree_everywhere`：后台、客户端的名字在四处写的是同一组——后台代码的两个常量、客户端找后台用的
/// 常量（去掉平台的可执行后缀）、清单脚本的 BINARIES、装配脚本的 PROGRAMS。改一处不改别处就红。
#[test]
fn binary_names_agree_everywhere() {
    let script_list = |file: &str, var: &str| -> Vec<String> {
        let text = std::fs::read_to_string(repo_root().join(file)).unwrap();
        let line = text
            .lines()
            .find(|l| l.starts_with(&format!("{var}=(")))
            .unwrap_or_else(|| panic!("{file} 里没有 {var}=(…)"));
        let inner = line
            .split_once('(')
            .and_then(|(_, r)| r.split_once(')'))
            .unwrap()
            .0;
        let mut names: Vec<String> = inner.split_whitespace().map(str::to_owned).collect();
        names.sort();
        names
    };
    let mut code = vec![DAEMON.to_owned(), CLIENT.to_owned()];
    code.sort();
    assert_eq!(
        script_list("scripts/write-install-manifest.sh", "BINARIES"),
        code
    );
    assert_eq!(script_list("scripts/assemble-release.sh", "PROGRAMS"), code);
    let launcher_daemon = iwork_client_core::launcher::DAEMON_FILE
        .strip_suffix(std::env::consts::EXE_SUFFIX)
        .unwrap_or(iwork_client_core::launcher::DAEMON_FILE);
    assert_eq!(launcher_daemon, DAEMON, "客户端找后台用的名字");
}

/// R3 `manifest_matches_final_files`：重算成品里两个程序的身份，等于清单里登记的。
#[test]
fn manifest_matches_final_files() {
    let i = Inputs::new("a");
    ok(&i.assemble());
    let bin = i.unsigned_bin();
    let manifest = Manifest::load(&bin.join(MANIFEST_FILE)).unwrap();
    for name in [DAEMON, CLIENT] {
        let listed = manifest.binaries.get(name).unwrap();
        let mut on_disk = file_image_ids(&bin.join(name), Want::All).unwrap();
        let mut want = listed.clone();
        on_disk.sort_by_key(ImageId::to_string);
        want.sort_by_key(ImageId::to_string);
        assert_eq!(on_disk, want, "{name}");
    }
}

/// R9 `assemble_requires_explicit_paths`：少给一个路径参数就是用法错（退出 2），不去找默认位置。
#[test]
fn assemble_requires_explicit_paths() {
    let i = Inputs::new("a");
    let mut cmd = Command::new(BASH);
    cmd.arg(repo_root().join("scripts/assemble-release.sh"));
    if cfg!(target_os = "macos") {
        cmd.arg("--app").arg(&i.app);
    }
    cmd.arg("--client")
        .arg(&i.client)
        .arg("--daemon")
        .arg(&i.daemon)
        .arg("--dist")
        .arg(&i.dist); // 少了 --check
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(!i.dist.exists(), "用法错时什么都不建");
}

/// R6 `failed_check_keeps_previous_artifact`：成品路径上先有一个好包；这次核对不过 → 非零退出，原成品一个字节都没动，
/// 没有留下暂存。
#[test]
fn failed_check_keeps_previous_artifact() {
    let i = Inputs::new("a");
    ok(&i.assemble());
    let before = digest_tree(&i.product());
    // 一个总说「不过」的核对程序
    let fake_check = i.root.join("check-fails");
    write_executable(&fake_check, "#!/bin/sh\necho 形态 FAIL 故意的\nexit 1\n");
    let out = i
        .command()
        .args(["--check"])
        .arg(&fake_check)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(digest_tree(&i.product()), before, "原成品没动");
    assert_eq!(
        dist_entries(&i.dist),
        [if cfg!(target_os = "macos") {
            "unsigned"
        } else {
            "linux"
        }]
    );
}

/// R6 `interrupted_run_never_exposes_unchecked`：
/// ① 核对当中被 kill -9：成品路径上是原来的好包，留下一个 `.staging-*`；
/// ② 再打一次：`.staging-*` 被清掉；
/// ③ 断在两次改名之间（旧成品已改名留底、新的还没到位）：下一次开头把留底改回成品；
/// ④ 新成品已到位、留底还在：下一次开头删留底。
/// （改名之间那两刻用盘上的样子造出来；杀在核对当中是真杀。）
#[test]
fn interrupted_run_never_exposes_unchecked() {
    let i = Inputs::new("a");
    ok(&i.assemble());
    let good = digest_tree(&i.product());
    let form_dir = i.product().parent().unwrap().to_path_buf();
    let name = i.product().file_name().unwrap().to_owned();

    // ① 核对卡住时杀
    let hang = i.root.join("check-hangs");
    let marker = i.root.join("check-started");
    write_executable(
        &hang,
        &format!("#!/bin/sh\ntouch '{}'\nexec sleep 30\n", marker.display()),
    );
    // 装配在锁里重新跑自己：杀整个进程组（只杀外面那层，锁里的那层会接着跑、一直拿着锁）
    let mut child = {
        use std::os::unix::process::CommandExt;
        i.command()
            .arg("--check")
            .arg(&hang)
            .process_group(0)
            .spawn()
            .unwrap()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline, "核对没开始");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // SAFETY：只对自己起的子进程发信号
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let _ = child.wait();
    assert_eq!(digest_tree(&i.product()), good, "成品路径上还是原来的好包");
    assert!(
        dist_entries(&i.dist)
            .iter()
            .any(|n| n.starts_with(".staging-")),
        "被 kill -9 时删不到暂存：{:?}",
        dist_entries(&i.dist)
    );
    // 卡住的 sleep 留着也不影响；收掉它
    let _ = Command::new("pkill")
        .arg("-f")
        .arg(hang.to_str().unwrap())
        .status();

    // ② 再打一次：暂存清掉，照常发布
    ok(&i.assemble());
    assert!(
        !dist_entries(&i.dist).iter().any(|n| n.starts_with('.')),
        "{:?}",
        dist_entries(&i.dist)
    );

    // ③ 断在两次改名之间：成品不在、留底在 → 改回去（用一个核对必然失败的运行来触发开头的恢复，不发布新的）。
    //   照脚本的次序造出来：先记下留底（身份就是成品的），再改名
    let current = digest_tree(&i.product());
    let form = form_dir.file_name().unwrap().to_str().unwrap().to_owned();
    journal_add(&i.dist, &format!("{form}/.old-99999"), &i.product());
    std::fs::rename(i.product(), form_dir.join(".old-99999")).unwrap();
    let fake_check = i.root.join("check-fails");
    write_executable(&fake_check, "#!/bin/sh\nexit 1\n");
    let out = i
        .command()
        .arg("--check")
        .arg(&fake_check)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(digest_tree(&i.product()), current, "留底改回了成品");
    assert!(!form_dir.join(".old-99999").exists());

    // ④ 成品在、留底也在（记录里有它）→ 删留底
    let leftover = form_dir.join(".old-88888");
    std::fs::create_dir_all(leftover.join("x")).unwrap();
    journal_add(&i.dist, &format!("{form}/.old-88888"), &leftover);
    let out = i
        .command()
        .arg("--check")
        .arg(&fake_check)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(!leftover.exists(), "留底被删");
    assert_eq!(digest_tree(&form_dir.join(&name)), current);
}

fn write_executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    /// 审查 1/F-1：Tauri 的包里 Contents/MacOS 是指向包外的目录链接 → 装配拒绝，包外的 iwork-daemon 没被改写。
    #[test]
    fn bundle_with_symlinked_dir_is_refused_and_outside_untouched() {
        let i = Inputs::new("a");
        let outside = i.root.join("outside-bin");
        let macos = i.app.join("Contents/MacOS");
        std::fs::rename(&macos, &outside).unwrap();
        std::fs::write(outside.join(DAEMON), "keep-original").unwrap();
        std::os::unix::fs::symlink(&outside, &macos).unwrap();
        let out = i.assemble();
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert!(stderr(&out).contains("有链接"), "{}", stderr(&out));
        assert_eq!(
            std::fs::read_to_string(outside.join(DAEMON)).unwrap(),
            "keep-original"
        );
        assert!(!i.product().exists());
    }

    /// R9 `assemble_stops_when_app_missing`：`--app` 指的位置没有包 → 停。
    #[test]
    fn assemble_stops_when_app_missing() {
        let i = Inputs::new("a");
        std::fs::remove_dir_all(&i.app).unwrap();
        let out = i.assemble();
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert!(
            stderr(&out).contains("找不到 Tauri 的包"),
            "{}",
            stderr(&out)
        );
    }

    /// R3 `assemble_stops_when_bundled_client_differs`：包里的主程序被改了一个字节 → 停，不发布。
    #[test]
    fn assemble_stops_when_bundled_client_differs() {
        let i = Inputs::new("a");
        let bundled = i.app.join("Contents/MacOS").join(CLIENT);
        let mut bytes = std::fs::read(&bundled).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&bundled, bytes).unwrap();
        let out = i.assemble();
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert!(stderr(&out).contains("不是同一份"), "{}", stderr(&out));
        assert!(!i.product().exists());
    }

    /// R5 `assemble_stops_on_presealed_bundle`：Tauri 的包已有封印 → 停。
    #[test]
    fn assemble_stops_on_presealed_bundle() {
        let i = Inputs::new("a");
        run(Command::new("codesign").args(["-f", "-s", "-"]).arg(&i.app));
        let out = i.assemble();
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert!(
            stderr(&out).contains("Tauri 给包签了名"),
            "{}",
            stderr(&out)
        );
        assert!(!i.product().exists());
    }

    fn signed_command(i: &Inputs, s: &Signer) -> Command {
        let mut cmd = i.command();
        cmd.env("IWORK_SIGN_IDENTITY", &s.identity)
            .env("IWORK_SIGN_KEYCHAIN", &s.keychain);
        cmd
    }

    /// R4 `signed_bundle_verifies` 与对照 `file_added_after_seal_fails_check`：形态 B 装配后整包验签过、核对三件都过；
    /// 成品里再放一个文件，核对不过。
    #[test]
    fn signed_bundle_verifies() {
        let i = Inputs::new("b");
        let s = Signer::new("iWork assemble signed");
        ok(&signed_command(&i, &s).output().unwrap());
        let app = i.dist.join("signed/iWork.app");
        run(Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&app));
        let bin = app.join("Contents/MacOS");
        assert!(!bin.join(MANIFEST_FILE).exists(), "签名的包里没有清单");
        let checked = Command::new(check_install())
            .arg(&bin)
            .arg("--expect-leaf")
            .arg(hex::encode(s.leaf))
            .output()
            .unwrap();
        ok(&checked);

        std::fs::write(app.join("Contents/Resources/extra.txt"), "多出来的").unwrap();
        let after = Command::new(check_install()).arg(&bin).output().unwrap();
        assert_eq!(after.status.code(), Some(1), "封印之后多一个文件：核对不过");
    }

    /// R2 `signed_form_refuses_leftover_manifest`：Tauri 的包里有一份清单（比如上一次的），形态 B 装配 → 停（I2）。
    #[test]
    fn signed_form_refuses_leftover_manifest() {
        let i = Inputs::new("b");
        let s = Signer::new("iWork assemble leftover");
        std::fs::write(i.app.join("Contents/MacOS").join(MANIFEST_FILE), "{}").unwrap();
        let out = signed_command(&i, &s).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert!(
            stderr(&out).contains("签名的包里不能有清单"),
            "{}",
            stderr(&out)
        );
        assert!(!i.dist.join("signed/iWork.app").exists());
    }

    /// R10 `adhoc_identity_fails_before_publish` 的前一半：`IWORK_SIGN_IDENTITY=-` 或空 → 第 0 步就停，`dist/` 里没有它。
    /// （后一半「绕过第 0 步、ad-hoc 整包签名的包过不了核对」在 tests/check_install.rs。）
    #[test]
    fn adhoc_identity_fails_before_publish() {
        for id in ["-", ""] {
            let i = Inputs::new("b");
            let out = i.command().env("IWORK_SIGN_IDENTITY", id).output().unwrap();
            assert_eq!(out.status.code(), Some(1), "{id:?}：{}", stderr(&out));
            assert!(stderr(&out).contains("ad-hoc"), "{}", stderr(&out));
            assert!(!i.dist.join("signed").exists());
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_refuses_sign_identity() {
    let i = Inputs::new("a");
    let out = i
        .command()
        .env("IWORK_SIGN_IDENTITY", "anything")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("没有签名的形态"));
}

/// macOS 自带的 bash 3.2 在 UTF-8 locale 下，会把紧跟在 `$变量` 后面的中文字节当成变量名的一部分（`$label）`
/// 读成一个叫 `label）` 的变量），set -u 下当场退出（T04p 审查 1/F-5）。仓库里所有 shell 脚本都不许这样写：
/// `$` 后的名字紧跟非 ASCII 字节时必须写成 `${名字}`。
#[test]
fn shell_vars_before_non_ascii_are_braced() {
    let files = run(Command::new("git")
        .args(["ls-files", "*.sh"])
        .current_dir(repo_root()));
    let mut bad = Vec::new();
    for f in files.lines() {
        let bytes = std::fs::read(repo_root().join(f)).unwrap();
        for (n, line) in bytes.split(|b| *b == b'\n').enumerate() {
            let mut i = 0;
            while i < line.len() {
                if line[i] == b'$'
                    && i + 1 < line.len()
                    && (line[i + 1].is_ascii_alphabetic() || line[i + 1] == b'_')
                {
                    let mut j = i + 1;
                    while j < line.len() && (line[j].is_ascii_alphanumeric() || line[j] == b'_') {
                        j += 1;
                    }
                    if j < line.len() && line[j] >= 0x80 {
                        bad.push(format!("{f}:{}", n + 1));
                    }
                    i = j;
                } else {
                    i += 1;
                }
            }
        }
    }
    assert!(files.lines().count() > 5, "对照：确实扫到了脚本");
    assert!(
        bad.is_empty(),
        "这些行的 $变量 后面紧跟非 ASCII 字节，要写成 ${{变量}}：{bad:?}"
    );
}

/// 审查 1/F-1：成品目录下的形态目录是指向别处的链接（别处有一个 .old-* 与它的内容）→ 装配拒绝，链接目标里的
/// 东西一个都没动。
#[test]
fn form_dir_symlink_is_refused_and_untouched() {
    let i = Inputs::new("a");
    let elsewhere = i.root.join("unrelated");
    std::fs::create_dir_all(elsewhere.join(".old-important")).unwrap();
    std::fs::write(elsewhere.join(".old-important/keep.txt"), "keep").unwrap();
    // 先正常装配一次：成品目录、形态目录都在装配的记录里；再把形态目录换成指向别处的链接
    ok(&i.assemble());
    let form = if cfg!(target_os = "macos") {
        "unsigned"
    } else {
        "linux"
    };
    std::fs::remove_dir_all(i.dist.join(form)).unwrap();
    std::os::unix::fs::symlink(&elsewhere, i.dist.join(form)).unwrap();
    let before = digest_tree(&elsewhere);
    let out = i.assemble();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("不在装配的记录里"),
        "{}",
        stderr(&out)
    );
    assert_eq!(digest_tree(&elsewhere), before, "链接目标没被动过");
}

/// 审查 1/F-1：成品目录本身已有别人的东西（不是装配建的、不是空的）→ 拒绝，不动。
#[test]
fn foreign_dist_dir_is_refused() {
    let i = Inputs::new("a");
    std::fs::create_dir_all(&i.dist).unwrap();
    std::fs::write(i.dist.join("someone-elses.txt"), "keep").unwrap();
    let out = i.assemble();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("没有本工具的记录"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(i.dist.join("someone-elses.txt")).unwrap(),
        "keep"
    );
}

/// 审查 2/F-1：别人的非空目录里恰好有叫 .iwork-owned 的普通文件（旧的归属标记），里面的 .staging-* 也有 → 标记不算
/// 证明：拒绝，整个目录原样留着。
#[test]
fn foreign_dir_with_marker_file_is_refused() {
    let i = Inputs::new("a");
    std::fs::create_dir_all(i.dist.join(".staging-1/keep")).unwrap();
    // 两层都放旧的标记文件：旧的认领办法会认下成品目录、把 .staging-1 当成上一次的暂存整个删掉（reviewer 实测）
    std::fs::write(i.dist.join(".iwork-owned"), "").unwrap();
    std::fs::write(i.dist.join(".staging-1/.iwork-owned"), "").unwrap();
    std::fs::write(i.dist.join(".staging-1/keep/file"), "keep").unwrap();
    let before = digest_tree(&i.dist);
    let out = i.assemble();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(digest_tree(&i.dist), before, "一个字节都没动");
}

/// 审查 2/F-1：成品目录有装配的记录，但里面有一个记录里没有的 .staging-*（同名不等于是装配建的）→ 不删，停下报错。
#[test]
fn unrecorded_staging_is_refused_and_kept() {
    let i = Inputs::new("a");
    ok(&i.assemble());
    std::fs::create_dir_all(i.dist.join(".staging-424242")).unwrap();
    std::fs::write(i.dist.join(".staging-424242/keep"), "keep").unwrap();
    let out = i.assemble();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("不在装配的记录里"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(i.dist.join(".staging-424242/keep")).unwrap(),
        "keep"
    );
}

/// 审查 2/F-1：形态目录是装配的，但里面有一个不在记录里的 .old-personal-backup（人放的备份），成品不在 → 不删、
/// 也不当成留底提升为成品；停下报错。
#[test]
fn unrecorded_backup_is_neither_deleted_nor_promoted() {
    let i = Inputs::new("a");
    ok(&i.assemble());
    let form_dir = i.product().parent().unwrap().to_path_buf();
    let backup = form_dir.join(".old-personal-backup");
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(backup.join("keep"), "keep").unwrap();
    // 成品在、成品不在两种情形都试：前者以前会删它，后者以前会把它改名成成品
    for remove_product in [false, true] {
        if remove_product {
            std::fs::remove_dir_all(i.product()).unwrap();
        }
        let out = i.assemble();
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert!(
            stderr(&out).contains("不在装配的记录里"),
            "{}",
            stderr(&out)
        );
        assert_eq!(
            std::fs::read_to_string(backup.join("keep")).unwrap(),
            "keep"
        );
        assert_eq!(i.product().exists(), !remove_product, "没把备份提升为成品");
    }
}

/// 审查 2/F-1：一个装配正在跑（核对卡住，它的暂存在记录里）时再起一个：拿不到锁就停，不收拾、不删正在用的暂存。
/// 对照：前一个结束后再起，照常发布。
#[test]
fn concurrent_assembly_is_refused_and_active_staging_kept() {
    let i = Inputs::new("a");
    let hang = i.root.join("check-hangs");
    let marker = i.root.join("check-started");
    let release = i.root.join("release");
    write_executable(
        &hang,
        &format!(
            "#!/bin/sh\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\nexit 1\n",
            marker.display(),
            release.display()
        ),
    );
    let mut first = i.command().arg("--check").arg(&hang).spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline, "核对没开始");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let staging: Vec<String> = dist_entries(&i.dist)
        .into_iter()
        .filter(|n| n.starts_with(".staging-"))
        .collect();
    assert_eq!(staging.len(), 1, "{staging:?}");
    let out = i.assemble();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("另一个装配正在用"),
        "{}",
        stderr(&out)
    );
    assert!(i.dist.join(&staging[0]).exists(), "正在用的暂存没被删");
    std::fs::write(&release, "").unwrap();
    assert_eq!(first.wait().unwrap().code(), Some(1));
    ok(&i.assemble());
}

/// 审查 3/F-2：只杀装配的主进程（不是整个进程组），它起的核对还活着、还在用暂存：锁要跟着还活着的进程走，第二次装配
/// 拿不到锁，不收拾、不删那份暂存。核对也结束之后，再装配就照记录收拾掉它、照常发布。
#[test]
fn killing_only_the_main_process_keeps_the_lock_while_children_live() {
    let i = Inputs::new("a");
    let hang = i.root.join("check-hangs");
    let marker = i.root.join("check-started");
    let release = i.root.join("release");
    write_executable(
        &hang,
        &format!(
            "#!/bin/sh\necho $PPID > '{}.tmp' && mv '{}.tmp' '{}'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\nexit 1\n",
            marker.display(),
            marker.display(),
            marker.display(),
            release.display()
        ),
    );
    let mut first = i.command().arg("--check").arg(&hang).spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline, "核对没开始");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // 核对记下的父进程就是跑装配主体的那个 bash：只杀它（不杀进程组、不杀核对）
    let body: i32 = std::fs::read_to_string(&marker)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY：只对这个测试起的装配主体发信号
    unsafe { libc::kill(body, libc::SIGKILL) };
    let _ = first.wait();
    let staging: Vec<String> = dist_entries(&i.dist)
        .into_iter()
        .filter(|n| n.starts_with(".staging-"))
        .collect();
    assert_eq!(staging.len(), 1, "{staging:?}");
    let out = i.assemble();
    // 先放掉核对，免得断言失败时它一直挂着
    std::fs::write(&release, "").unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("另一个装配正在用"),
        "{}",
        stderr(&out)
    );
    assert!(i.dist.join(&staging[0]).exists(), "还在用的暂存没被删");
    // 核对结束（锁随最后一个持有者释放）之后：照记录收拾、照常发布
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let out = i.assemble();
        if out.status.success() {
            break;
        }
        assert!(
            stderr(&out).contains("另一个装配正在用"),
            "{}",
            stderr(&out)
        );
        assert!(std::time::Instant::now() < deadline, "锁一直没放");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        !i.dist.join(&staging[0]).exists(),
        "残留的暂存按记录收拾掉了"
    );
}
