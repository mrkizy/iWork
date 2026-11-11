//! iWork T04 N2（方案第 8 节）：std::time::Instant 用的是哪个钟、这个钟在系统睡眠期间走不走。
//! 用 rustc 直接编译（不依赖任何 crate），由 iwork-t04-sleep-clock.sh 调用。
//!
//! 做法：同一时刻读 Instant（Debug 里有它的 tv_sec）与几个系统钟，看 Instant 和哪个钟对得上；
//! 再看「计睡眠的钟 − 不计睡眠的钟」——开机以来系统睡过多久，这个差就是多少。
//! macOS：CLOCK_UPTIME_RAW（不计睡眠）对 CLOCK_MONOTONIC_RAW（计睡眠，= mach_continuous_time）。
//! Linux：CLOCK_MONOTONIC（不计挂起）对 CLOCK_BOOTTIME（计挂起）。
//! 带参数 `watch N` 时每秒打一行，N 秒后退出：机器在这期间睡过的话，可直接看到 Instant 少走了多少。
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}
unsafe extern "C" {
    fn clock_gettime(clk: i32, ts: *mut Timespec) -> i32;
}

#[cfg(target_os = "macos")]
const CLOCKS: [(&str, i32); 3] = [("UPTIME_RAW", 8), ("MONOTONIC", 6), ("MONOTONIC_RAW", 4)];
#[cfg(target_os = "macos")]
const PAIR: (&str, &str) = ("UPTIME_RAW", "MONOTONIC_RAW");
#[cfg(target_os = "linux")]
const CLOCKS: [(&str, i32); 3] = [("MONOTONIC", 1), ("MONOTONIC_RAW", 4), ("BOOTTIME", 7)];
#[cfg(target_os = "linux")]
const PAIR: (&str, &str) = ("MONOTONIC", "BOOTTIME");

fn read(clk: i32) -> f64 {
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: ts 是本栈帧里的可写内存，布局与 C 的 struct timespec 相同（64 位平台）。
    assert_eq!(unsafe { clock_gettime(clk, &mut ts) }, 0);
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

fn instant_secs(i: Instant) -> f64 {
    // Debug 形如 Instant { t: Timespec { tv_sec: 1, tv_nsec: 2 } }：std 没有公开读数，只能这样取。
    let s = format!("{i:?}");
    let num = |key: &str| -> f64 {
        let at = s.find(key).unwrap_or_else(|| panic!("Instant 的 Debug 里没有 {key}：{s}")) + key.len();
        s[at..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap()
    };
    num("tv_sec: ") + num("tv_nsec: ") / 1e9
}

fn line() -> String {
    let inst = instant_secs(Instant::now());
    let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64();
    let values: Vec<(&str, f64)> = CLOCKS.iter().map(|(n, c)| (*n, read(*c))).collect();
    let matched: Vec<&str> = values.iter().filter(|(_, v)| (v - inst).abs() < 0.05).map(|(n, _)| *n).collect();
    let get = |n: &str| values.iter().find(|(k, _)| *k == n).unwrap().1;
    let slept = get(PAIR.1) - get(PAIR.0);
    let clocks: Vec<String> = values.iter().map(|(n, v)| format!("{n}={v:.3}")).collect();
    format!(
        "wall={wall:.3} instant={inst:.3} {} instant_matches={} slept_since_boot({}-{})={slept:.3}",
        clocks.join(" "),
        if matched.is_empty() { "none".to_string() } else { matched.join("|") },
        PAIR.1,
        PAIR.0
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("{}", line());
    if args.first().map(String::as_str) == Some("watch") {
        let n: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60);
        let (i0, w0) = (Instant::now(), SystemTime::now());
        for _ in 0..n {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let di = i0.elapsed().as_secs_f64();
            let dw = SystemTime::now().duration_since(w0).unwrap().as_secs_f64();
            println!("instant_elapsed={di:.3} wall_elapsed={dw:.3} wall_minus_instant={:.3}", dw - di);
        }
    }
}
