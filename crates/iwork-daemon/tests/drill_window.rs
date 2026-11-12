//! 回退窗口一格怎么判（`scripts/lib/drill-window.sh` 的 `window_verdict`，T04p 方案 9.8；审查 1/F-6）。
//! 直接喂证据行（「日志行号 ts 标签」），用 macOS 自带的 bash 3.2 跑：成功样本过，相等、乱序、未就绪、映像不符等反例不过。
#![cfg(unix)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const BASH: &str = if cfg!(target_os = "macos") {
    "/bin/bash"
} else {
    "bash"
};

/// 判一格：期望、t1、上限、B 就绪、t4 之后的映像，加证据行。返回输出的第一个词与整行。
fn verdict(expect: &str, t1: u64, ready: &str, image: &str, rows: &str) -> (String, String) {
    let lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/lib/drill-window.sh");
    let mut child = Command::new(BASH)
        .arg("-c")
        .arg(format!(
            "set -euo pipefail; source '{}'; window_verdict \"$@\"",
            lib.display()
        ))
        .arg("sh")
        .args([expect, &t1.to_string(), "10000", ready, image])
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(rows.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let word = line.split_whitespace().next().unwrap_or("").to_owned();
    (word, line)
}

/// 方案 9.8 的形态 B 一格：t2 连上、t3 重启、t4 重新连上。
const CONNECTED_OK: &str = "10 1500 connected\n12 2000 restart\n14 3000 connected\n";
/// 形态 A 一格：t2 被拒，紧接着立刻检查，t3 重启、t4 连上。
const REJECTED_OK: &str =
    "10 1500 rejected\n11 1500 reject_check\n12 2000 restart\n14 3000 connected\n";

#[test]
fn complete_samples_pass() {
    assert_eq!(
        verdict("connected", 1000, "yes", "A", CONNECTED_OK).0,
        "PASS"
    );
    assert_eq!(verdict("rejected", 1000, "yes", "A", REJECTED_OK).0, "PASS");
}

/// 证据缺、自相矛盾 → INCONCLUSIVE（不是 PASS，也不冤枉成 FAIL）。
#[test]
fn contradictory_or_missing_evidence_is_inconclusive() {
    let cases: &[(&str, u64, &str, &str, &str)] = &[
        ("t1 与 t2 相等", 1500, "yes", "A", CONNECTED_OK),
        (
            "t2 与 t3 相等",
            1000,
            "yes",
            "A",
            "10 2000 connected\n12 2000 restart\n14 3000 connected\n",
        ),
        (
            "t3 与 t4 相等",
            1000,
            "yes",
            "A",
            "10 1500 connected\n12 2000 restart\n14 2000 connected\n",
        ),
        (
            "日志里 t3 在 t2 之前、时刻却是 t2<t3",
            1000,
            "yes",
            "A",
            "11 2000 restart\n12 1500 connected\n14 3000 connected\n",
        ),
        (
            "t4 的时刻早于 t3",
            1000,
            "yes",
            "A",
            "10 1500 connected\n12 2000 restart\n14 1800 connected\n",
        ),
        ("回退前 B 版没就绪", 1000, "no", "A", CONNECTED_OK),
        ("没有重启", 1000, "yes", "A", "10 1500 connected\n"),
        (
            "重启之后没再连上",
            1000,
            "yes",
            "A",
            "10 1500 connected\n12 2000 restart\n",
        ),
        (
            "日志丢了记录",
            1000,
            "yes",
            "A",
            "10 1500 connected\n11 1600 dropped\n12 2000 restart\n14 3000 connected\n",
        ),
        ("取不到 t4 之后的映像", 1000, "yes", "unknown", CONNECTED_OK),
    ];
    for (name, t1, ready, image, rows) in cases {
        let (word, line) = verdict("connected", *t1, ready, image, rows);
        assert_eq!(word, "INCONCLUSIVE", "{name}：{line}");
    }
}

/// 证据齐全但行为不对 → FAIL。
#[test]
fn wrong_behaviour_fails() {
    let cases: &[(&str, &str, &str, &str)] = &[
        ("t4 之后跑的不是 A 版", "connected", "other", CONNECTED_OK),
        ("期望连上却被拒", "connected", "A", REJECTED_OK),
        ("期望被拒却连上", "rejected", "A", CONNECTED_OK),
        (
            "被拒之后没有立刻检查",
            "rejected",
            "A",
            "10 1500 rejected\n12 2000 restart\n14 3000 connected\n",
        ),
        (
            "立刻检查记在被拒之前",
            "rejected",
            "A",
            "9 1400 reject_check\n10 1500 rejected\n12 2000 restart\n14 3000 connected\n",
        ),
        (
            "t1 到 t4 超过上限",
            "connected",
            "A",
            "10 1500 connected\n12 2000 restart\n14 99000 connected\n",
        ),
    ];
    for (name, expect, image, rows) in cases {
        let (word, line) = verdict(expect, 1000, "yes", image, rows);
        assert_eq!(word, "FAIL", "{name}：{line}");
    }
}

/// 审查 2/F-3：附加事件只按各自的区间算（9.8）：立刻检查在 t2..t3，W1 的被拒在 t1..t3，丢记录在 t1..t4，W2 重连后不许再被拒。
#[test]
fn side_events_count_only_inside_their_interval() {
    let cases: &[(&str, &str, &str, &str)] = &[
        (
            "W2：立刻检查记在 t4 之后",
            "rejected",
            "FAIL",
            "10 1500 rejected\n12 2000 restart\n14 3000 connected\n15 3100 reject_check\n",
        ),
        (
            "W2：立刻检查记在重启之后、重连之前",
            "rejected",
            "FAIL",
            "10 1500 rejected\n12 2000 restart\n13 2500 reject_check\n14 3000 connected\n",
        ),
        (
            "W2：重连之后又被拒",
            "rejected",
            "FAIL",
            "10 1500 rejected\n11 1500 reject_check\n12 2000 restart\n14 3000 connected\n15 3200 rejected\n",
        ),
        (
            "W1：先连上、重启之前又被拒",
            "connected",
            "FAIL",
            "10 1500 connected\n11 1700 rejected\n12 2000 restart\n14 3000 connected\n",
        ),
        (
            "t3 与 t4 之间丢了记录",
            "connected",
            "INCONCLUSIVE",
            "10 1500 connected\n12 2000 restart\n13 2500 dropped\n14 3000 connected\n",
        ),
        (
            "对照：t4 之后才丢记录，窗口照样完整",
            "connected",
            "PASS",
            "10 1500 connected\n12 2000 restart\n14 3000 connected\n15 3100 dropped\n",
        ),
        (
            "对照：W2 的检查在 t2 与 t3 之间",
            "rejected",
            "PASS",
            "10 1500 rejected\n11 1600 reject_check\n12 2000 restart\n14 3000 connected\n",
        ),
    ];
    for (name, expect, want, rows) in cases {
        let (word, line) = verdict(expect, 1000, "yes", "A", rows);
        assert_eq!(&word, want, "{name}：{line}");
    }
}
