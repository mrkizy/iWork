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

impl Drop for Layout {
    /// 恢复权限，临时目录才删得掉。
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                self.root.join("ws/gate"),
                std::fs::Permissions::from_mode(0o755),
            );
        }
    }
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
            fake.file(root.join(rel));
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
            // 末级是链接：指向 .git/config 的文件链接、指向 .git 的目录链接
            ("ws/c", PathBuf::from(".git/config")),
            ("ws/g", PathBuf::from(".git")),
        ];
        for (at, target) in links {
            symlink(target, &root.join(at));
            fake.link(root.join(at), target.clone());
        }
        // 没有搜索权限的目录（查不清）：真实文件系统上 chmod 000，假的标 Denied
        mkdir(&mut fake, "ws/gate/root");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join("ws/gate"), std::fs::Permissions::from_mode(0o000))
                .unwrap();
            fake.deny(root.join("ws/gate"));
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

    /// 期望的规范路径：大小写不敏感的文件系统上整条折成小写（与解析器同一规则，2/F-7）。
    fn p(&self, rel: &str) -> CanonicalPath {
        let p = self.root.join(rel);
        if OsPathResolver::detect_case_insensitive(&self.root).unwrap() {
            CanonicalPath(PathBuf::from(p.to_string_lossy().to_lowercase()))
        } else {
            CanonicalPath(p)
        }
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
            .unwrap()
            .target;
    assert_eq!(r, l.p("outside/secret.txt"));
    assert!(!l.os.is_under(&r, &l.ws()));
    // 对照：留在工作区内的 ..
    let r = l.os.resolve(&l.ws(), "src/../src/a.rs").unwrap().target;
    assert!(l.os.is_under(&r, &l.ws()));
}

#[test]
fn symlink_out_of_workspace() {
    let l = Layout::new();
    let r = l.os.resolve(&l.ws(), "out/secret.txt").unwrap().target;
    assert_eq!(r, l.p("outside/secret.txt"));
    assert!(!l.os.is_under(&r, &l.ws()));
    let r = l.os.resolve(&l.ws(), "keys/id_rsa").unwrap().target;
    assert!(l.os.is_under(&r, &l.p("home/.ssh")));
    // 还不存在的文件：真实前缀之后原样接上
    let r = l.os.resolve(&l.ws(), "keys/new_key").unwrap().target;
    assert_eq!(r, l.p("home/.ssh/new_key"));
}

/// F-9：`..` 作用在已解析的真实前缀上：`objs/../config` 是 `.git/config`，不是 `config`。
#[test]
fn dotdot_after_symlink_uses_real_parent() {
    let l = Layout::new();
    let r = l.os.resolve(&l.ws(), "objs/../config").unwrap().target;
    assert_eq!(r, l.p("ws/.git/config"));
    assert!(l.os.is_under(&r, &l.p("ws/.git")));
}

#[test]
fn case_variant() {
    let l = Layout::new();
    let r = l.os.resolve(&l.ws(), ".GIT/CONFIG").unwrap().target;
    if OsPathResolver::detect_case_insensitive(&l.root).unwrap() {
        // 整条折成小写：写法不同的同一路径得到同一个键
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
    let r = l.os.resolve(&l.ws(), "../alias/x").unwrap().target;
    assert_eq!(r, l.p("real/x"));
    // macOS 的 /tmp 是 /private/tmp 的链接
    #[cfg(target_os = "macos")]
    {
        let r = l.os.resolve(&l.ws(), "/tmp/iwork-x").unwrap().target;
        assert!(
            l.os.same_path(&r, &PathBuf::from("/private/tmp/iwork-x")),
            "{r:?}"
        );
    }
}

#[test]
fn home_and_errors() {
    let l = Layout::new();
    assert_eq!(
        l.os.resolve(&l.ws(), "~/.ssh/id_rsa").unwrap().target,
        l.p("home/.ssh/id_rsa")
    );
    assert_eq!(l.os.resolve(&l.ws(), "~").unwrap().target, l.p("home"));
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
        "c",
        "g",
        "g/",
        "g/.",
        "g/config",
        "out",
        "../ws/c",
        "loop1",
        "",
        "/",
    ];
    let mut raws = raws.to_vec();
    // Windows 上普通文件后再接分量报「找不到」而不是「不是目录」，权限也没有 chmod 000：只在 unix 对拍
    #[cfg(unix)]
    raws.extend(["src/a.rs/x", "gate/root", "gate"]);
    for raw in raws {
        let os = l.os.resolve(&l.ws(), raw);
        let fake = l.fake.resolve(&l.ws(), raw);
        match (&os, &fake) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "{raw:?}"),
            // 权限错误的文字随平台不同，只比种类
            (Err(a), Err(b)) => assert_eq!(
                std::mem::discriminant(a),
                std::mem::discriminant(b),
                "{raw:?}：{a} / {b}"
            ),
            _ => panic!("{raw:?}：真实 {os:?}，假的 {fake:?}"),
        }
    }
}

/// 末级是链接时两种视图不同（方案 2.3）：`entry` 是链接所在位置，`target` 是链接指向。
#[test]
fn last_component_link_has_two_views() {
    let l = Layout::new();
    let r = l.os.resolve(&l.ws(), "c").unwrap();
    assert_eq!(r.entry, l.p("ws/c"));
    assert_eq!(r.target, l.p("ws/.git/config"));
    // 中间分量的链接照常跟随，末级不是链接时两种视图相同
    let r = l.os.resolve(&l.ws(), "g/config").unwrap();
    assert_eq!(r.entry, l.p("ws/.git/config"));
    assert_eq!(r.target, l.p("ws/.git/config"));
    // 对照：普通文件
    let r = l.os.resolve(&l.ws(), "src/a.rs").unwrap();
    assert_eq!(r.entry, r.target);
}

/// 带尾斜杠、末级是 `.` 时内核先跟随末级：两种视图都是链接指向（9/F-1）。
#[test]
fn trailing_slash_follows_last_link() {
    let l = Layout::new();
    for raw in ["g/", "g/."] {
        let r = l.os.resolve(&l.ws(), raw).unwrap();
        assert_eq!(r.entry, l.p("ws/.git"), "{raw}");
        assert_eq!(r.target, l.p("ws/.git"), "{raw}");
    }
    // 对照：不带尾斜杠时 entry 是链接本身
    assert_eq!(l.os.resolve(&l.ws(), "g").unwrap().entry, l.p("ws/g"));
}

/// 前缀分量是普通文件：走不通（`NotADirectory`）；没有搜索权限的目录：查不清（Io）。
#[test]
fn not_a_directory_and_permission_are_classified() {
    use iwork_ports::fs::ResolveCertainty;
    let l = Layout::new();
    #[cfg(unix)]
    {
        let e = l.os.resolve(&l.ws(), "src/a.rs/x").unwrap_err();
        assert!(matches!(e, PathResolveError::NotADirectory { .. }), "{e:?}");
        assert_eq!(e.certainty(), ResolveCertainty::Disproved);
        let e = l.os.resolve(&l.ws(), "gate/root").unwrap_err();
        assert!(matches!(e, PathResolveError::Io { .. }), "{e:?}");
        assert_eq!(e.certainty(), ResolveCertainty::Unknown);
        // 目录自己做末级时照常
        assert!(l.os.resolve(&l.ws(), "gate").is_ok());
    }
    let e = l.os.resolve(&l.ws(), "nope/../x").unwrap_err();
    assert_eq!(e.certainty(), ResolveCertainty::Disproved);
    let e = l.os.resolve(&l.ws(), "loop1/x").unwrap_err();
    assert_eq!(e.certainty(), ResolveCertainty::Unknown);
}

/// 2/F-7：同一路径的规范写法不随父目录是否存在而变（大小写不敏感时整条折叠；敏感时原样）。
/// 拒绝 `Write DIR/New.TXT` 之后先 `mkdir DIR` 再原样重试，指纹键必须相同。
#[test]
fn canonical_key_stable_across_parent_creation() {
    let l = Layout::new();
    let before = l.os.resolve(&l.ws(), "NEWDIR/New.TXT").unwrap();
    std::fs::create_dir(l.root.join("ws/NEWDIR")).unwrap();
    let after = l.os.resolve(&l.ws(), "NEWDIR/New.TXT").unwrap();
    assert_eq!(before, after);
    // 再建出文件本身也不变
    std::fs::write(l.root.join("ws/NEWDIR/New.TXT"), b"x").unwrap();
    assert_eq!(l.os.resolve(&l.ws(), "NEWDIR/New.TXT").unwrap(), before);
    if OsPathResolver::detect_case_insensitive(&l.root).unwrap() {
        // 换大小写写同一个文件：同一个键
        assert_eq!(l.os.resolve(&l.ws(), "newdir/new.txt").unwrap(), before);
    }
}
