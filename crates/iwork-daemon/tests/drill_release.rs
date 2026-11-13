//! 真包演练脚本 `scripts/drill-release-bundle.sh` 的安全性质（T04p 方案 R11–R13）。演练本身要真包、专用账号、人在场；
//! 这里把它要用的系统命令换成假程序（`launchctl`、`pgrep`、`open`、`lsof`、`ps`，包目录里的 `check-install`、
//! `ipcprobe`），在临时 HOME 里跑，证：前置检查会拒、数据根被钉死、取证读不到凭据、日志缺一类就停、失败照样收尾。
//! 假的 `open` 只按它收到的 `IWORK_DATA_DIR` 写后台日志：数据根没钉住，日志就写到别处，演练等不到记录。
//! 默认的假后台只是一个长睡的进程（不会自己重启，跑到第 2 类记录那一步就停）；状态里放了 `reactive` 时换成会看磁盘的
//! 假后台：装上的包变了就过一会儿记「自行重启」「就绪」、换上新映像，客户端还开着就重新连上；配上假的 `codesign`
//! 按映像报 CDHash。这样演练能跑完全程（U1–U4、9.6–9.8）到成功收尾（审查 1/F-8）。
//! 假后台替真后台写的那几行（如 `manifest_loaded self_listed=true`）只证脚本的流程；真后台确实写这一行由
//! `process::install::startup_manifest_audit_is_in_daemon_log` 守（字段与验收 2 的过滤条件相同）。
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn write_exe(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// 一个假的世界：临时 HOME、假命令目录、四个假包、状态目录（假命令按它回答）、调用记录。
struct World {
    _t: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    pkgs: PathBuf,
    state: PathBuf,
    calls: PathBuf,
    out: PathBuf,
    /// 外部哨兵：一个扮成「终端里留着的别的数据目录」的目录
    outer: PathBuf,
}

const SECRET: &str = "FAKESECRET-9f3c1e7a";

impl World {
    fn new() -> World {
        let t = tempfile::Builder::new()
            .prefix("iwd")
            .tempdir_in("/tmp")
            .unwrap();
        let root = t.path().canonicalize().unwrap();
        let w = World {
            home: root.join("home"),
            bin: root.join("bin"),
            pkgs: root.join("pkgs"),
            state: root.join("state"),
            calls: root.join("calls.log"),
            out: root.join("out"),
            outer: root.join("outer-data"),
            _t: t,
        };
        std::fs::create_dir_all(&w.home).unwrap();
        std::fs::create_dir_all(w.state.join("pids")).unwrap();
        std::fs::create_dir_all(w.state.join("foreign")).unwrap();
        std::fs::create_dir_all(&w.outer).unwrap();
        std::fs::write(w.outer.join("marker"), "outer").unwrap();
        for p in ["a-unsigned", "a-signed", "b-unsigned", "b-signed"] {
            let macos = w.pkgs.join(p).join("iWork.app/Contents/MacOS");
            std::fs::create_dir_all(&macos).unwrap();
            std::fs::write(macos.join("iwork-daemon"), p).unwrap();
        }
        w.write_fakes();
        w
    }

    fn write_fakes(&self) {
        let (st, calls, bin) = (
            self.state.display(),
            self.calls.display(),
            self.bin.display(),
        );
        let ms = "perl -MTime::HiRes=time -e 'printf \"%d\", time*1000'";
        write_exe(
            &self.bin.join("launchctl"),
            &format!(
                r#"#!/bin/sh
echo "launchctl $*" >> '{calls}'
case "$1" in
  print) echo "environment = {{ ANTHROPIC_API_KEY => {SECRET} }}"; [ -f '{st}/registered' ] && exit 0; exit 113 ;;
  getenv) if [ "$2" = IWORK_DATA_DIR ]; then cat '{st}/launchd_data_dir' 2>/dev/null; else echo {SECRET}; fi; exit 0 ;;
  bootout) rm -f '{st}/registered'; exit 0 ;;
esac
exit 0
"#
            ),
        );
        write_exe(
            &self.bin.join("pgrep"),
            &format!(
                r#"#!/bin/sh
echo "pgrep $*" >> '{calls}'
# 照 macOS 的 pgrep：不带 -u 时匹配所有用户的进程，带 -u 只认那个用户。pids/ 是本用户的，foreign/ 扮别的用户的
exact= uid= name=
while [ $# -gt 0 ]; do
  case "$1" in
    -x) exact=1; shift ;;
    # 真 pgrep 会把多个 -u 合成一个集合；替身不模拟，见到第二个就报错，免得只认最后一个而假绿
    -u) [ -z "$uid" ] || {{ echo "pgrep 替身：重复的 -u" >&2; exit 2; }}; uid="$2"; shift 2 ;;
    -*) exit 2 ;;
    *) name="$1"; shift ;;
  esac
done
[ -n "$exact" ] && [ -n "$name" ] || exit 2
emit() {{ for p in $(cat "$1" 2>/dev/null); do kill -0 "$p" 2>/dev/null && echo "$p"; done; }}
[ -z "$uid" ] || [ "$uid" = "$(id -u)" ] && emit '{st}/pids/'"$name"
[ "$uid" = "$(id -u)" ] || emit '{st}/foreign/'"$name"
exit 0
"#
            ),
        );
        // 假的 open：记下全部参数；按收到的 IWORK_DATA_DIR 定后台日志的位置；起两个长睡的进程当客户端与后台
        write_exe(
            &self.bin.join("open"),
            &format!(
                r#"#!/bin/sh
echo "open $*" >> '{calls}'
d=""; for a in "$@"; do case "$a" in IWORK_DATA_DIR=*) d="${{a#IWORK_DATA_DIR=}}" ;; esac; done
[ -n "$d" ] || d="$IWORK_DATA_DIR"
echo "$d" > '{st}/opened_root'
if [ -f '{st}/reactive' ]; then
  sleep 0.05
  mkdir -p "$d/logs" "$d/run"
  dest="$HOME/Applications/iWork.app/Contents/MacOS/iwork-daemon"
  dpid=$(cat '{st}/pids/iwork-daemon' 2>/dev/null)
  if [ -z "$dpid" ] || ! kill -0 "$dpid" 2>/dev/null; then
    cat "$dest" > '{st}/image'
    '{bin}/fake-daemon' "$d" >/dev/null 2>&1 & echo $! > '{st}/pids/iwork-daemon'
    sleep 0.1
  fi
  sleep 300 >/dev/null 2>&1 & echo $! > '{st}/pids/iwork-client'
  client=$(cat '{st}/pids/iwork-client')
  sleep 0.02
  ts=$({ms})
  # 形态 A（不签名）的客户端连不上不同版的后台：被拒，后台立刻做一次安装检查；形态 B 先连上
  case "$(cat "$dest")" in *-unsigned) unsigned=1 ;; *) unsigned= ;; esac
  if [ -n "$unsigned" ] && [ "$(cat '{st}/image')" != "$(cat "$dest")" ]; then
    echo '{{"ts":'$ts',"target":"iwork.ipc.audit","fields":{{"event":"peer_rejected","role":"server","pid":'$client'}}}}' >> "$d/logs/daemon.log"
    echo '{{"ts":'$ts',"target":"iwork_daemon::tasks::install_watch","fields":{{"message":"有客户端被拒，立刻做一次安装检查"}}}}' >> "$d/logs/daemon.log"
  else
    echo '{{"ts":'$ts',"target":"iwork_ipc","fields":{{"message":"连接已建立","pid":'$client'}}}}' >> "$d/logs/daemon.log"
  fi
  exit 0
fi
sleep 300 >/dev/null 2>&1 & echo $! > '{st}/pids/iwork-client'
sleep 300 >/dev/null 2>&1 & echo $! > '{st}/pids/iwork-daemon'
client=$(cat '{st}/pids/iwork-client')
mkdir -p "$d/logs" "$d/run"
ts=$({ms})
if [ ! -f '{st}/no_connected' ]; then
  echo '{{"ts":'$ts',"target":"iwork_ipc","fields":{{"message":"连接已建立","pid":'$client'}}}}' >> "$d/logs/daemon.log"
fi
echo '{{"ts":'$ts',"target":"iwork.ipc.audit","fields":{{"event":"manifest_loaded","self_listed":true}}}}' >> "$d/logs/daemon.log"
exit 0
"#
            ),
        );
        write_exe(
            &self.bin.join("lsof"),
            &format!(
                r#"#!/bin/sh
echo "lsof $*" >> '{calls}'
d=$(cat '{st}/opened_root')
[ -f '{st}/lsof_outside' ] && d=/elsewhere/iWork
[ -f '{st}/lsof_substring' ] && d="/unexpected$d"
echo "p$2"; echo "n$d/iwork.db"; echo "n$d/run/daemon.lock"; echo "n$d/logs/daemon.log"
"#
            ),
        );
        write_exe(
            &self.bin.join("ps"),
            &format!(
                r#"#!/bin/sh
echo "ps $*" >> '{calls}'
for a in "$@"; do case "$a" in -E|e|-e|*e*=*) ;; -*E*|[!-]*e*) echo "env {SECRET}"; exit 0 ;; esac; done
case "$2" in
  comm=) echo "$HOME/Applications/iWork.app/Contents/MacOS/iwork-daemon" ;;
  args=) echo "$HOME/Applications/iWork.app/Contents/MacOS/iwork-daemon" ;;
esac
"#
            ),
        );
        // 会看磁盘的假后台（只在 reactive 时由假 open 起）：起来记「就绪」；装上的包变了，过 0.3 秒记「自行重启」、
        // 换上新映像、记「就绪」，客户端还开着就记它重新连上。每条之间隔 20 毫秒，时刻严格递增
        write_exe(
            &self.bin.join("fake-daemon"),
            &format!(
                r#"#!/bin/bash
d=$1
dest="$HOME/Applications/iWork.app/Contents/MacOS/iwork-daemon"
log="$d/logs/daemon.log"
line() {{ echo '{{"ts":'$({ms})','"$1"'}}' >> "$log"; sleep 0.02; }}
unsigned_manifest() {{ case "$(cat '{st}/image')" in *-unsigned) line '"target":"iwork.ipc.audit","fields":{{"event":"manifest_loaded","self_listed":true}}' ;; esac; }}
line '"target":"iwork_daemon","fields":{{"message":"后台服务就绪"}}'
unsigned_manifest
while :; do
  cur=$(cat "$dest" 2>/dev/null || true)
  if [ -n "$cur" ] && [ "$cur" != "$(cat '{st}/image')" ]; then
    sleep 0.3
    line '"target":"iwork_daemon::tasks::install_watch","fields":{{"message":"磁盘上装齐了另一版，自行重启"}}'
    cat "$dest" > '{st}/image'
    line '"target":"iwork_daemon","fields":{{"message":"后台服务就绪"}}'
    unsigned_manifest
    c=$(cat '{st}/pids/iwork-client' 2>/dev/null)
    if [ -n "$c" ] && kill -0 "$c" 2>/dev/null; then
      line '"target":"iwork_ipc","fields":{{"message":"连接已建立","pid":'$c'}}'
    fi
  fi
  sleep 0.1
done
"#
            ),
        );
        // 假 codesign：给进程号就按假后台当前的映像、给路径就按文件内容报 CDHash；另外吐一行假凭据，取证要是整份记下来就会露
        write_exe(
            &self.bin.join("codesign"),
            &format!(
                r#"#!/bin/sh
echo "codesign $*" >> '{calls}'
for a in "$@"; do last=$a; done
case "$last" in
  *[!0-9]*) content=$(cat "$last" 2>/dev/null) ;;
  *) content=$(cat '{st}/image' 2>/dev/null) ;;
esac
echo "Authority={SECRET}" >&2
[ -n "$content" ] && echo "CDHash=$(printf %s "$content" | shasum | cut -c1-40)" >&2
exit 0
"#
            ),
        );
        write_exe(&self.pkgs.join("check-install"), "#!/bin/sh\nexit 0\n");
        // 探针：被后台拒一次，后台记 peer_rejected 与「立刻检查」（除非状态里关掉了）
        write_exe(
            &self.pkgs.join("ipcprobe"),
            &format!(
                r#"#!/bin/sh
d=$(dirname "$(dirname "$2")")
[ -f '{st}/no_reject' ] && exit 1
ts=$({ms})
echo '{{"ts":'$ts',"target":"iwork.ipc.audit","fields":{{"event":"peer_rejected","role":"server","pid":1}}}}' >> "$d/logs/daemon.log"
echo '{{"ts":'$ts',"target":"iwork_daemon::tasks::install_watch","fields":{{"message":"有客户端被拒，立刻做一次安装检查"}}}}' >> "$d/logs/daemon.log"
exit 1
"#
            ),
        );
    }

    fn flag(&self, name: &str) {
        std::fs::write(self.state.join(name), "").unwrap();
    }

    /// 跑演练：HOME 是临时的；终端里「留着」一个外部数据目录与一个假凭据；回答一串 y。
    fn drill(&self) -> Output {
        let path = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", self.bin.display());
        // 用系统自带的 bash 3.2、UTF-8 locale 跑：演练就是在这样的终端里跑的（审查 1/F-5）
        let mut child = Command::new("/bin/bash")
            .arg(repo_root().join("scripts/drill-release-bundle.sh"))
            .args(["--packages"])
            .arg(&self.pkgs)
            .arg("--out")
            .arg(&self.out)
            .env_clear()
            .env("HOME", &self.home)
            .env("LANG", "en_US.UTF-8")
            .env("LC_ALL", "en_US.UTF-8")
            .env("PATH", path)
            .env("IWORK_DATA_DIR", &self.outer)
            .env("ANTHROPIC_API_KEY", SECRET)
            .env("IWORK_DRILL_SETTLE_SECS", "0")
            .env("IWORK_DRILL_WAIT_SECS", "2")
            .env("IWORK_DRILL_STOP_SECS", "2")
            .env("IWORK_DRILL_COOLDOWN_SECS", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            let mut stdin = child.stdin.take().unwrap();
            let _ = stdin.write_all("y\n".repeat(20).as_bytes());
        }
        child.wait_with_output().unwrap()
    }

    fn record(&self) -> String {
        std::fs::read_to_string(self.out.join("record.txt")).unwrap_or_default()
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(&self.calls).unwrap_or_default()
    }

    fn data_root(&self) -> PathBuf {
        self.home.join("Library/Application Support/iWork")
    }

    fn outer_listing(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.outer)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // 假客户端、假后台（长睡进程）一律收掉
        for name in ["iwork-client", "iwork-daemon"] {
            if let Ok(text) = std::fs::read_to_string(self.state.join("pids").join(name)) {
                for p in text.split_whitespace() {
                    let _ = Command::new("kill").args(["-KILL", p]).output();
                }
            }
        }
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// R11：开始前自启就注册着 → 在动任何东西之前停（退出 3），不代为注销、不起进程。
#[test]
fn drill_refuses_when_label_registered() {
    let w = World::new();
    w.flag("registered");
    let out = w.drill();
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(
        w.record().contains("开始前的自启：注册着"),
        "{}",
        w.record()
    );
    assert!(!w.calls().contains("bootout"), "不代为注销");
    assert!(!w.calls().contains("open "), "没起任何进程");
    assert!(!w.data_root().exists());
}

/// R11：已有 iWork 进程在跑 → 停（退出 3）。
#[test]
fn drill_refuses_when_daemon_running() {
    let w = World::new();
    let mut running = Command::new("sleep").arg("30").spawn().unwrap();
    std::fs::write(w.state.join("pids/iwork-daemon"), running.id().to_string()).unwrap();
    let out = w.drill();
    let _ = running.kill();
    let _ = running.wait();
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(w.record().contains("在跑"), "{}", w.record());
    assert!(!w.calls().contains("open "));
}

/// R11：launchd 的用户环境里 IWORK_DATA_DIR 指向别处 → 停；记录里只有三态，不写值。
#[test]
fn drill_refuses_when_launchd_env_sets_data_dir() {
    let w = World::new();
    std::fs::write(w.state.join("launchd_data_dir"), "/some/other/place").unwrap();
    let out = w.drill();
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(w.record().contains("别的值"), "{}", w.record());
    assert!(!w.record().contains("/some/other/place"), "不写值");
    assert!(!w.calls().contains("open "));
}

/// R11 `drill_pins_data_root_against_inherited_env`：终端里留着 IWORK_DATA_DIR=<外部目录>。交给 open 的、前置检查查的、
/// 收尾改名的都是 HOME 下默认位置的同一个 D；外部目录的内容没变。对照：假 open 按收到的值写日志，所以要是脚本把外部
/// 目录传了下去，日志会写进外部目录、演练等不到第 1 类记录。
#[test]
fn drill_pins_data_root_against_inherited_env() {
    let w = World::new();
    let before = w.outer_listing();
    let out = w.drill();
    let d = w.data_root();
    let calls = w.calls();
    let open_line = calls
        .lines()
        .find(|l| l.starts_with("open "))
        .unwrap_or_else(|| panic!("没起客户端：{}", text(&out)));
    assert!(
        open_line.contains(&format!("IWORK_DATA_DIR={}", d.display())),
        "交给 open 的是 D：{open_line}"
    );
    assert_eq!(
        std::fs::read_to_string(w.state.join("opened_root"))
            .unwrap()
            .trim(),
        d.display().to_string()
    );
    assert_eq!(w.outer_listing(), before, "外部目录没变");
    // 走到了第 2 类记录那一步（假后台不会自己重启）才停：前面的第 1、3 类都在 D 下等到了
    assert_eq!(out.status.code(), Some(4), "{}", text(&out));
    assert!(w.record().contains("没放行第 2 类"), "{}", w.record());
    // 收尾把 D 改名留证，装上的包删了
    assert!(!d.exists(), "D 改名留证了");
    let parent = d.parent().unwrap();
    assert!(
        std::fs::read_dir(parent).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("iWork.drill-")),
        "留证的数据根在"
    );
    assert!(!w.home.join("Applications/iWork.app").exists());
}

/// R11：后台打开了 D 之外的库文件 → 走收尾、退出 1、记录里点出那个路径。
#[test]
fn drill_fails_when_daemon_opens_outside_root() {
    let w = World::new();
    w.flag("lsof_outside");
    let out = w.drill();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        w.record().contains("/elsewhere/iWork/iwork.db"),
        "{}",
        w.record()
    );
    assert!(
        !w.home.join("Applications/iWork.app").exists(),
        "收尾照样删了装上的包"
    );
}

/// R11 `drill_cleanup_runs_on_failure`：中途失败，收尾照样执行：它起的进程都退出了，装的包删了，D 改名留证。
#[test]
fn drill_cleanup_runs_on_failure() {
    let w = World::new();
    w.flag("lsof_outside");
    let _ = w.drill();
    for name in ["iwork-client", "iwork-daemon"] {
        let pid = std::fs::read_to_string(w.state.join("pids").join(name)).unwrap();
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .status()
            .unwrap()
            .success();
        assert!(!alive, "{name} 被收尾结束了");
    }
    assert!(
        w.record().contains("结束时的自启：没注册"),
        "{}",
        w.record()
    );
}

/// R13 `drill_stops_when_log_class_missing`：三类记录各缺一类，脚本都在自检处停（退出 4）、走收尾，写明缺第几类。
#[test]
fn drill_stops_when_log_class_missing() {
    for (flag, class) in [("no_connected", 1), ("no_reject", 3)] {
        let w = World::new();
        w.flag(flag);
        let out = w.drill();
        assert_eq!(out.status.code(), Some(4), "{flag}：{}", text(&out));
        assert!(
            w.record().contains(&format!("没放行第 {class} 类")),
            "{}",
            w.record()
        );
        assert!(!w.record().contains("9.7"), "后面的格一格都没做");
    }
    // 第 2 类缺：假后台不会自己重启，任何一次跑到那一步都停在这里（见 drill_pins_data_root_against_inherited_env）
}

/// R12 `drill_record_has_no_env_secrets`：环境、launchctl print 的输出、getenv 别的名字、ps 带 e 都会吐出一个假凭据；
/// 演练（失败一遍、前置检查拒一遍）的输出目录和终端输出里都搜不到它；ps 没被带着 e 调过；getenv 只问过 IWORK_DATA_DIR。
#[test]
fn drill_record_has_no_env_secrets() {
    let failing = World::new();
    failing.flag("lsof_outside");
    let refused = World::new();
    refused.flag("registered");
    for w in [&failing, &refused] {
        let out = w.drill();
        assert_no_secret(w, &out);
    }
}

/// 独立核对一次演练没读、没记假凭据：终端输出与输出目录里每个文件都搜不到；ps 没带 e 调过；getenv 只问过
/// IWORK_DATA_DIR；脚本自己的虚构凭据标记检查也没响。
fn assert_no_secret(w: &World, out: &Output) {
    assert!(!text(out).contains(SECRET), "终端输出里有假凭据");
    let mut stack = vec![w.out.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let bytes = std::fs::read(&p).unwrap();
                assert!(
                    !String::from_utf8_lossy(&bytes).contains(SECRET),
                    "{} 里有假凭据",
                    p.display()
                );
            }
        }
    }
    for line in w.calls().lines() {
        if let Some(args) = line.strip_prefix("ps ") {
            assert!(
                !args
                    .split_whitespace()
                    .any(|a| a == "e" || a == "-E" || a == "-e"),
                "{line}"
            );
        }
        if let Some(rest) = line.strip_prefix("launchctl getenv ") {
            assert_eq!(rest.trim(), "IWORK_DATA_DIR", "{line}");
        }
    }
    assert!(!w.record().contains("搜到了虚构凭据标记"), "{}", w.record());
}

/// R12 + 审查 1/F-8：会看磁盘的假后台让演练跑完全程（U1–U4、9.6、9.7 四遍、9.8 两格）到成功收尾。各格的判定是 PASS，
/// 收尾删了装上的包与留底、数据根改名留证；整个过程同样读不到、记不下假凭据（后半段的取证也在覆盖里）。
#[test]
fn drill_success_run_has_no_secrets_and_cleans_up() {
    let w = World::new();
    w.flag("reactive");
    let out = w.drill();
    let record = w.record();
    assert_eq!(out.status.code(), Some(0), "{}\n{record}", text(&out));
    assert!(record.contains("演练跑完"), "{record}");
    assert!(record.contains("U1 PASS"), "{record}");
    for round in [
        "b-signed→a-signed",
        "a-signed→b-signed",
        "b-signed→b-unsigned",
        "b-unsigned→a-signed",
    ] {
        assert!(
            record.contains(&format!("9.7 {round} PASS")),
            "{round}：{record}"
        );
    }
    assert!(record.contains("W1 PASS"), "{record}");
    assert!(record.contains("W2 PASS"), "{record}");
    assert!(!record.contains("FAIL："), "没有记成失败的格：{record}");
    assert_no_secret(&w, &out);
    let apps = w.home.join("Applications");
    for name in [
        "iWork.app",
        "iWork.prev.app",
        "iWork.incoming.app",
        "iWork.failed.app",
    ] {
        assert!(!apps.join(name).exists(), "{name} 收尾删了");
    }
    assert!(!w.data_root().exists(), "数据根改名留证了");
}

/// 找进程只认本用户（审查后补）：macOS 的 pgrep 不带 -u 会匹配所有用户的进程。别的用户开着同名的 iwork-daemon、
/// iwork-client 时，演练照样从前置检查跑到成功收尾，别人的进程没被选中、没被杀；每次找进程都带着 -u 本用户。
#[test]
fn drill_ignores_other_users_processes() {
    let w = World::new();
    w.flag("reactive");
    let mut foreign = Vec::new();
    for name in ["iwork-daemon", "iwork-client"] {
        let child = Command::new("sleep").arg("120").spawn().unwrap();
        std::fs::write(w.state.join("foreign").join(name), child.id().to_string()).unwrap();
        foreign.push(child);
    }
    let out = w.drill();
    let record = w.record();
    let calls = w.calls();
    let alive: Vec<bool> = foreign
        .iter_mut()
        .map(|c| c.try_wait().unwrap().is_none())
        .collect();
    for mut c in foreign {
        let _ = c.kill();
        let _ = c.wait();
    }
    assert_eq!(out.status.code(), Some(0), "{}\n{record}", text(&out));
    assert!(record.contains("演练跑完"), "{record}");
    assert!(!record.contains("FAIL："), "{record}");
    assert_eq!(alive, [true, true], "别的用户的进程不归演练收：{record}");
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    let pgreps: Vec<&str> = calls.lines().filter(|l| l.starts_with("pgrep ")).collect();
    assert!(!pgreps.is_empty());
    for l in pgreps {
        let args: Vec<&str> = l.split_whitespace().collect();
        let users: Vec<&str> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| **a == "-u")
            .map(|(i, _)| args.get(i + 1).copied().unwrap_or(""))
            .collect();
        assert_eq!(users, [uid.trim()], "找进程要恰好一个 -u，值是本用户：{l}");
    }
}

/// 审查 1/F-2：Applications 下已经有别人的 iWork.prev.app / iWork.incoming.app / iWork.failed.app → 前置检查停（退出 3），
/// 不起进程，那几个目录原样留着。
#[test]
fn drill_refuses_foreign_app_siblings_and_keeps_them() {
    for name in ["iWork.prev.app", "iWork.incoming.app", "iWork.failed.app"] {
        let w = World::new();
        let keep = w.home.join("Applications").join(name).join("keep.txt");
        std::fs::create_dir_all(keep.parent().unwrap()).unwrap();
        std::fs::write(&keep, "keep").unwrap();
        let out = w.drill();
        assert_eq!(out.status.code(), Some(3), "{name}：{}", text(&out));
        assert!(w.record().contains(name), "{}", w.record());
        assert!(!w.calls().contains("open "), "没起任何进程");
        assert_eq!(
            std::fs::read_to_string(&keep).unwrap(),
            "keep",
            "{name} 原样留着"
        );
    }
}

/// 审查 1/F-2：`--out` 指向已有目录（里面的 record.txt 是指到别处的链接）→ 不用它（退出 3），链接指向的文件没被截断。
#[test]
fn drill_refuses_existing_out_dir_and_leaves_link_target() {
    let w = World::new();
    std::fs::create_dir_all(&w.out).unwrap();
    let target = w.state.join("outside-record");
    std::fs::write(&target, "precious").unwrap();
    std::os::unix::fs::symlink(&target, w.out.join("record.txt")).unwrap();
    let out = w.drill();
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "precious");
    assert!(!w.calls().contains("launchctl"), "什么都没查、没动");
}

/// 审查 1/F-7：lsof 报的库在 `/unexpected<D>/` 下（路径里含 D、但不在 D 里）→ 演练失败、点出那个路径。
#[test]
fn drill_fails_when_open_path_only_contains_root_as_substring() {
    let w = World::new();
    w.flag("lsof_substring");
    let out = w.drill();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(w.record().contains("数据根之外"), "{}", w.record());
    assert!(w.record().contains("/unexpected"), "{}", w.record());
}

/// R12 `drill_script_has_no_env_dump_commands`（第二道，读脚本文本）：没有会吐整份环境的命令；getenv 只问 IWORK_DATA_DIR。
#[test]
fn drill_script_has_no_env_dump_commands() {
    let script =
        std::fs::read_to_string(repo_root().join("scripts/drill-release-bundle.sh")).unwrap();
    for (n, line) in script.lines().enumerate() {
        let code = line.split('#').next().unwrap_or("").trim();
        if code.is_empty() {
            continue;
        }
        let words: Vec<&str> = code.split_whitespace().collect();
        let has = |w: &str| words.contains(&w);
        assert!(!has("printenv"), "第 {} 行：{line}", n + 1);
        assert!(!(has("export") && has("-p")), "第 {} 行：{line}", n + 1);
        assert!(
            !code.contains("launchctl export"),
            "第 {} 行：{line}",
            n + 1
        );
        assert!(!code.contains("set -x"), "第 {} 行：{line}", n + 1);
        assert!(
            !code.starts_with("env") && !code.contains("| env") && !code.contains("$(env"),
            "第 {} 行：{line}",
            n + 1
        );
        // ps 的参数：到同一行下一个管道为止（后面是别的命令的参数）
        if let Some(pos) = words.iter().position(|w| *w == "ps") {
            for a in words[pos + 1..].iter().take_while(|a| **a != "|") {
                assert!(
                    !(a.starts_with('-') && (a.contains('e') || a.contains('E'))),
                    "第 {} 行：{line}",
                    n + 1
                );
            }
        }
        if let Some(rest) = code.split("launchctl getenv ").nth(1) {
            assert!(
                rest.starts_with("IWORK_DATA_DIR"),
                "第 {} 行：{line}",
                n + 1
            );
        }
    }
}

/// R13 `drill_log_spec_keeps_three_classes`：从假 open 记下的参数里取出脚本实际传的 IWORK_LOG，交给后台自己的解析：
/// `iwork_ipc` 的 debug、`install_watch` 的 info 都放行。对照：只写 `iwork_ipc=debug` 时 install_watch 的 info 不放行。
#[test]
fn drill_log_spec_keeps_three_classes() {
    use tracing::Level;
    let w = World::new();
    let _ = w.drill();
    let calls = w.calls();
    let spec = calls
        .lines()
        .find(|l| l.starts_with("open "))
        .and_then(|l| {
            l.split_whitespace()
                .find_map(|a| a.strip_prefix("IWORK_LOG="))
        })
        .expect("交给 open 的参数里有 IWORK_LOG")
        .to_owned();
    let watch = "iwork_daemon::tasks::install_watch";
    let filter = iwork_daemon::logging::filter_from(Some(&spec));
    assert!(
        filter.would_enable("iwork_ipc", &Level::DEBUG),
        "第 1 类：{spec}"
    );
    assert!(
        filter.would_enable(watch, &Level::INFO),
        "第 2、3 类：{spec}"
    );
    let narrow = iwork_daemon::logging::filter_from(Some("iwork_ipc=debug"));
    assert!(narrow.would_enable("iwork_ipc", &Level::DEBUG));
    assert!(
        !narrow.would_enable(watch, &Level::INFO),
        "对照：没有默认级别时 install_watch 的 info 被滤掉"
    );
}
