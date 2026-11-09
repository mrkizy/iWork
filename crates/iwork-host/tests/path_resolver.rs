//! `OsPathResolver` 在真实文件系统上的行为（T07 验收 5、方案 S5），以及与 `FakeFs` 的对拍：
//! 同一组目录与链接在两边搭一遍，同一组输入的解析结果必须相同——核心的测试全部用 `FakeFs`，
//! 它的行为由这里证明与真实文件系统一致。

#[path = "../../iwork-ports/tests/fake_fs/mod.rs"]
mod fake_fs;

use std::path::{Path, PathBuf};

use fake_fs::FakeFs;
use iwork_domain::action::CanonicalPath;
use iwork_host::fs::OsPathResolver;
use iwork_ports::fs::{PathResolveError, PathResolver};

/// 临时目录里的一套布局：`home/`、`ws/`（工作区，含 `.git/objects`、`src/a.rs`）、`outside/secret.txt`、
/// 以及几条链接。返回 (根, 真实解析器, 同构的假文件系统)。
struct Layout {
    _dir: tempfile::TempDir,
    root: PathBuf,
    os: OsPathResolver,
    fake: FakeFs,
}

impl Layout {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ci = OsPathResolver::detect_case_insensitive(&root).unwrap();
        let home = root.join("home");
        let mut fake = FakeFs::new(&home, ci);
        let mkdir = |fake: &mut FakeFs, rel: &str| {
            std::fs::create_dir_all(root.join(rel)).unwrap();
            fake.add(root.join(rel));
        };
        let touch = |fake: &mut FakeFs, rel: &str| {
            std::fs::write(root.join(rel), b"x").unwrap();
            fake.add(root.join(rel));
        };
        mkdir(&mut fake, "home/.ssh");
        touch(&mut fake, "home/.ssh/id_rsa");
        mkdir(&mut fake, "ws/.git/objects");
        touch(&mut fake, "ws/.git/config");
        mkdir(&mut fake, "ws/src");
        touch(&mut fake, "ws/src/a.rs");
        mkdir(&mut fake, "outside");
        touch(&mut fake, "outside/secret.txt");
        mkdir(&mut fake, "real");
        // 链接：指向工作区外（绝对）、指向 .git/objects（相对）、指向凭据目录、别名目录、成环
        let links: &[(&str, PathBuf)] = &[
            ("ws/out", root.join("outside")),
            ("ws/objs", PathBuf::from(".git/objects")),
            ("ws/keys", root.join("home/.ssh")),
            ("alias", PathBuf::from("real")),
            ("ws/loop1", PathBuf::from("loop2")),
            ("ws/loop2", PathBuf::from("loop1")),
        ];
        for (at, target) in links {
            symlink(target, &root.join(at));
            fake.link(root.join(at), target.clone());
        }
        let os = OsPathResolver::new(home.canonicalize().unwrap(), ci);
        Self {
            _dir: dir,
            root,
            os,
            fake,
        }
    }

    fn ws(&self) -> CanonicalPath {
        CanonicalPath(self.root.join("ws"))
    }

    fn p(&self, rel: &str) -> CanonicalPath {
        CanonicalPath(self.root.join(rel))
    }
}

#[cfg(unix)]
fn symlink(target: &Path, at: &Path) {
    std::os::unix::fs::symlink(target, at).unwrap();
}

#[cfg(windows)]
fn symlink(target: &Path, at: &Path) {
    // 目录链接；没有开发者模式时建不出来，测试按失败报出（CI 的 Windows 作业有权限）
    std::os::windows::fs::symlink_dir(target, at).unwrap();
}

#[test]
fn dotdot_escapes_workspace() {
    let l = Layout::new();
    let r =
        l.os.resolve(&l.ws(), "src/../../outside/secret.txt")
            .unwrap();
    assert_eq!(r, l.p("outside/secret.txt"));
    assert!(!l.os.is_under(&r, &l.ws()));
    // 对照：留在工作区内的 ..
    let r = l.os.resolve(&l.ws(), "src/../src/a.rs").unwrap();
    assert!(l.os.is_under(&r, &l.ws()));
}

#[test]
fn symlink_out_of_workspace() {
    let l = Layout::new();
    let r = l.os.resolve(&l.ws(), "out/secret.txt").unwrap();
    assert_eq!(r, l.p("outside/secret.txt"));
    assert!(!l.os.is_under(&r, &l.ws()));
    let r = l.os.resolve(&l.ws(), "keys/id_rsa").unwrap();
    assert!(l.os.is_under(&r, &l.p("home/.ssh")));
    // 还不存在的文件：真实前缀之后原样接上
    let r = l.os.resolve(&l.ws(), "keys/new_key").unwrap();
    assert_eq!(r, l.p("home/.ssh/new_key"));
}

/// F-9：`..` 作用在已解析的真实前缀上：`objs/../config` 是 `.git/config`，不是 `config`。
#[test]
fn dotdot_after_symlink_uses_real_parent() {
    let l = Layout::new();
    let r = l.os.resolve(&l.ws(), "objs/../config").unwrap();
    assert_eq!(r, l.p("ws/.git/config"));
    assert!(l.os.is_under(&r, &l.p("ws/.git")));
}

#[test]
fn case_variant() {
    let l = Layout::new();
    let r = l.os.resolve(&l.ws(), ".GIT/CONFIG").unwrap();
    if OsPathResolver::detect_case_insensitive(&l.root).unwrap() {
        // 已存在的分量取磁盘上的大小写
        assert_eq!(r, l.p("ws/.git/config"));
        assert!(l.os.same_path(&l.p("ws/.GIT"), &l.p("ws/.git")));
        assert!(l.os.is_under(&l.p("ws/.Git/NEW"), &l.p("ws/.git")));
    } else {
        assert_eq!(r, l.p("ws/.GIT/CONFIG"));
        assert!(!l.os.is_under(&r, &l.p("ws/.git")));
    }
}

#[test]
fn alias_directory() {
    let l = Layout::new();
    let r = l.os.resolve(&l.ws(), "../alias/x").unwrap();
    assert_eq!(r, l.p("real/x"));
    // macOS 的 /tmp 是 /private/tmp 的链接
    #[cfg(target_os = "macos")]
    {
        let r = l.os.resolve(&l.ws(), "/tmp/iwork-x").unwrap();
        assert_eq!(r.0, PathBuf::from("/private/tmp/iwork-x"));
    }
}

#[test]
fn home_and_errors() {
    let l = Layout::new();
    assert_eq!(
        l.os.resolve(&l.ws(), "~/.ssh/id_rsa").unwrap(),
        l.p("home/.ssh/id_rsa")
    );
    assert_eq!(l.os.resolve(&l.ws(), "~").unwrap(), l.p("home"));
    assert!(matches!(
        l.os.resolve(&l.ws(), "~root/x"),
        Err(PathResolveError::OtherUserHome { .. })
    ));
    assert!(matches!(
        l.os.resolve(&l.ws(), "nope/../x"),
        Err(PathResolveError::DotDotAfterMissing { .. })
    ));
    assert!(matches!(
        l.os.resolve(&l.ws(), "loop1/x"),
        Err(PathResolveError::SymlinkLoop { .. })
    ));
    assert_eq!(l.os.resolve(&l.ws(), ""), Err(PathResolveError::Empty));
}

/// 对拍：同构的 `FakeFs` 对每个输入给出与真实文件系统相同的结果（成功值与错误种类都比）。
#[test]
fn fake_fs_agrees_with_os() {
    let l = Layout::new();
    let raws = [
        "src/a.rs",
        "src/../../outside/secret.txt",
        "out/secret.txt",
        "out/../ws/src",
        "keys/id_rsa",
        "keys/new",
        "objs/../config",
        "objs/../../src/a.rs",
        ".GIT/CONFIG",
        "./src/./a.rs",
        "../alias/x/y",
        "~/.ssh",
        "~/.ssh/../.ssh/id_rsa",
        "~",
        "~root",
        "missing/../x",
        "loop1",
        "",
        "/",
    ];
    for raw in raws {
        let os = l.os.resolve(&l.ws(), raw);
        let fake = l.fake.resolve(&l.ws(), raw);
        match (&os, &fake) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "{raw:?}"),
            (Err(a), Err(b)) => assert_eq!(
                std::mem::discriminant(a),
                std::mem::discriminant(b),
                "{raw:?}：{a} / {b}"
            ),
            _ => panic!("{raw:?}：真实 {os:?}，假的 {fake:?}"),
        }
    }
}
