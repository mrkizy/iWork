//! 把 `git clone --progress` 的输出折算成单调递增的总进度（纯函数，无 I/O）。

use iwork_ports::vcs::CloneProgress;

/// 克隆的各阶段，及它在总进度里占的区间 `[start, end]`。
///
/// 按字节量粗估：接收对象占大头，远端计数与压缩、解析增量、检出各占一小段。git 不报的阶段（例如
/// 浅克隆没有解析增量）直接跳过，总进度只会往前跳，不会倒退。
const PHASES: &[(&str, &str, u8, u8)] = &[
    // (git 输出里的阶段名, 对外的阶段键, 起点, 终点)
    ("Counting objects", "counting", 0, 5),
    ("Compressing objects", "compressing", 5, 10),
    ("Receiving objects", "receiving", 10, 80),
    ("Resolving deltas", "resolving", 80, 95),
    ("Updating files", "checkout", 95, 99),
];

/// 克隆成功结束时的阶段键。
pub(crate) const DONE_PHASE: &str = "done";

/// 逐行喂 git 的输出，只在总进度变大时吐出一条进度，所以吐出的序列严格递增。
#[derive(Debug, Default)]
pub(crate) struct ProgressParser {
    last_percent: Option<u8>,
}

impl ProgressParser {
    /// 喂一行（已按 `\r` / `\n` 切开）。是进度行且总进度变大时返回进度。
    pub(crate) fn feed(&mut self, line: &str) -> Option<CloneProgress> {
        let (phase, percent) = parse_line(line)?;
        self.advance(phase, percent, line.trim())
    }

    /// 克隆成功后补一条 100%。
    pub(crate) fn finish(&mut self) -> Option<CloneProgress> {
        self.advance(DONE_PHASE, 100, "done")
    }

    fn advance(&mut self, phase: &str, percent: u8, text: &str) -> Option<CloneProgress> {
        if self.last_percent.is_some_and(|last| percent <= last) {
            return None;
        }
        self.last_percent = Some(percent);
        Some(CloneProgress {
            phase: phase.to_owned(),
            percent,
            text: text.to_owned(),
        })
    }
}

/// 是不是一行进度：返回 (阶段键, 总进度)。
pub(crate) fn parse_line(line: &str) -> Option<(&'static str, u8)> {
    let line = line.trim();
    let line = line.strip_prefix("remote:").map_or(line, str::trim_start);
    let (name, rest) = line.split_once(':')?;
    let &(_, key, start, end) = PHASES.iter().find(|(n, ..)| *n == name)?;
    let phase_percent: u32 = rest.trim_start().split_once('%')?.0.parse().ok()?;
    let phase_percent = phase_percent.min(100);
    let span = u32::from(end - start);
    // 结果不超过 end（<= 99），一定装得进 u8
    let overall = u32::from(start) + span * phase_percent / 100;
    Some((key, u8::try_from(overall).ok()?))
}

/// 是不是进度行（失败时留错误输出要跳过它们）。
pub(crate) fn is_progress_line(line: &str) -> bool {
    parse_line(line).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn maps_each_phase_into_its_span() {
        assert_eq!(
            parse_line("remote: Counting objects: 100% (3/3), done.        "),
            Some(("counting", 5))
        );
        assert_eq!(
            parse_line("Receiving objects:  50% (1/2)"),
            Some(("receiving", 45))
        );
        assert_eq!(
            parse_line("Resolving deltas: 100% (2/2), done."),
            Some(("resolving", 95))
        );
        assert_eq!(parse_line("Cloning into 'x'..."), None);
        assert_eq!(parse_line("remote: Enumerating objects: 3, done."), None);
        assert_eq!(parse_line("fatal: repository not found"), None);
    }

    #[test]
    fn emits_only_when_overall_progress_grows() {
        let mut p = ProgressParser::default();
        let got: Vec<u8> = [
            "Receiving objects:  10% (1/10)",
            "Receiving objects:  10% (1/10)",
            "remote: Counting objects: 100% (3/3), done.",
            "Receiving objects:  20% (2/10)",
        ]
        .into_iter()
        .filter_map(|l| p.feed(l))
        .map(|e| e.percent)
        .collect();
        assert_eq!(got, vec![17, 24]);
        assert_eq!(p.finish().map(|e| e.percent), Some(100));
        assert_eq!(p.finish(), None);
    }

    fn arbitrary_line() -> impl Strategy<Value = String> {
        let phase = prop::sample::select(vec![
            "Counting objects",
            "Compressing objects",
            "Receiving objects",
            "Resolving deltas",
            "Updating files",
            "Bogus",
        ]);
        prop_oneof![
            (phase, 0u32..=150, any::<bool>()).prop_map(|(n, pct, remote)| {
                let prefix = if remote { "remote: " } else { "" };
                format!("{prefix}{n}: {pct}% (1/2)")
            }),
            ".*",
        ]
    }

    proptest! {
        /// 任何输出顺序（阶段乱序、百分比回退、杂行）下，吐出的进度都严格递增且不超过 100。
        #[test]
        fn emitted_progress_strictly_increases(lines in prop::collection::vec(arbitrary_line(), 0..64)) {
            let mut p = ProgressParser::default();
            let mut emitted: Vec<u8> = lines.iter().filter_map(|l| p.feed(l)).map(|e| e.percent).collect();
            emitted.extend(p.finish().map(|e| e.percent));
            prop_assert!(emitted.windows(2).all(|w| w[0] < w[1]), "{emitted:?}");
            prop_assert!(emitted.iter().all(|&x| x <= 100));
            prop_assert_eq!(emitted.last().copied(), Some(100));
        }
    }
}
