//! 续接摘要的单测与属性测试（T06 方案 15.3「续接失败用摘要新开」纯函数一条）。期望值手写。

use iwork_ports::read_model::{SummaryLine, SummarySource, SummaryText, SummaryWho};
use proptest::prelude::*;

use super::*;

fn user(text: &str) -> SummaryLine {
    SummaryLine {
        who: SummaryWho::User,
        text: SummaryText::Inline(text.into()),
        attachments: Vec::new(),
    }
}

fn bot(text: &str) -> SummaryLine {
    SummaryLine {
        who: SummaryWho::Assistant {
            name: "小助手".into(),
        },
        text: SummaryText::Inline(text.into()),
        attachments: Vec::new(),
    }
}

/// 整份的样子（快照）：开头、第一条、按时间正序的最近几条；附件只留名字，存成制品的只记一行。
#[test]
fn summary_snapshot() {
    let mut with_file = user("看看这个日志");
    with_file.attachments = vec!["build.log".into(), "a.png".into()];
    let mut stored = bot("");
    stored.text = SummaryText::Stored;
    let source = SummarySource {
        first_user: Some(user("帮我把登录页改成深色")),
        // 从最新往前
        earlier: vec![stored, with_file, bot("改好了，跑过 npm test")],
        unfetched: 0,
        request: Some(user("再加一个按钮")),
    };
    assert_eq!(
        resume_summary(&source, SummaryLimits::DEFAULT).unwrap(),
        "[iWork] 以下是此前对话的摘要。其中提到的操作都已完成，不要重做。\n\n\
         用户：帮我把登录页改成深色\n\n\
         助手（小助手）：改好了，跑过 npm test\n\n\
         用户：看看这个日志（附件：build.log、a.png）\n\n\
         助手（小助手）：（一条较长的回复，未收入摘要）"
    );
}

/// 这一轮的请求不进摘要正文；续接时单独给，原文照给。
#[test]
fn request_is_not_in_summary() {
    let source = SummarySource {
        first_user: Some(user("第一句")),
        earlier: vec![bot("好的")],
        unfetched: 0,
        request: Some(user("这一轮的请求ZZZ")),
    };
    let summary = resume_summary(&source, SummaryLimits::DEFAULT).unwrap();
    assert!(!summary.contains("ZZZ"), "{summary}");
    assert!(summary.contains("第一句"), "对照：摘要确实有内容");
    let long = "长".repeat(5_000);
    assert_eq!(
        request_reminder(&user(&long)),
        format!("[iWork] 这一轮用户的请求是：{long}"),
        "请求原文不截断"
    );
}

/// 此前什么都没有 → 不发摘要；只有第一条用户消息也算有上文。
#[test]
fn empty_line_gives_none() {
    let empty = SummarySource {
        request: Some(user("第一句就是这一轮")),
        ..SummarySource::default()
    };
    assert_eq!(resume_summary(&empty, SummaryLimits::DEFAULT), None);
    let only_first = SummarySource {
        first_user: Some(user("上一轮没回上")),
        ..SummarySource::default()
    };
    assert!(resume_summary(&only_first, SummaryLimits::DEFAULT).is_some());
}

/// 装不下时从最新往前装，第一条一定在，中间省略的条数 = 没取的 + 装不下的。单条超长截尾加标记。
#[test]
fn truncates_from_the_middle() {
    let limits = SummaryLimits::new(400, 100);
    let earlier: Vec<_> = (0..20)
        .rev()
        .map(|i| bot(&format!("第{i}条{}", "字".repeat(40))))
        .collect();
    let source = SummarySource {
        first_user: Some(user(&"首".repeat(300))),
        earlier,
        unfetched: 7,
        request: None,
    };
    let out = resume_summary(&source, limits).unwrap();
    assert!(out.chars().count() <= 400, "{}", out.chars().count());
    let parts: Vec<&str> = out.split("\n\n").collect();
    assert_eq!(parts[0], SUMMARY_HEADER);
    // 第一条截到 100 字符，末尾是标记
    assert_eq!(parts[1].chars().count(), 100);
    assert!(parts[1].starts_with("用户：首") && parts[1].ends_with("…（已截断）"));
    // 每条 ~51 字符：除去开头、第一条与省略行的预留，装得下 4 条（第 16–19 条）
    let kept: Vec<&str> = parts[3..].to_vec();
    assert_eq!(kept.len(), 4, "{parts:?}");
    assert!(
        kept[0].contains("第16条") && kept[3].contains("第19条"),
        "按时间正序：{kept:?}"
    );
    assert_eq!(parts[2], format!("（中间省略 {} 条）", 7 + 16));
}

/// 全装得下、也没有没取的：不插省略行。
#[test]
fn no_omission_line_when_nothing_skipped() {
    let source = SummarySource {
        first_user: Some(user("a")),
        earlier: vec![bot("b")],
        unfetched: 0,
        request: None,
    };
    let out = resume_summary(&source, SummaryLimits::DEFAULT).unwrap();
    assert!(!out.contains("省略"), "{out}");
    let skipped = SummarySource {
        unfetched: 2,
        ..source
    };
    assert!(
        resume_summary(&skipped, SummaryLimits::DEFAULT)
            .unwrap()
            .contains("（中间省略 2 条）")
    );
}

#[test]
#[should_panic(expected = "摘要上限装不下")]
fn limits_must_fit_first_and_newest() {
    let _ = SummaryLimits::new(100, 100);
}

fn arb_line() -> impl Strategy<Value = SummaryLine> {
    (
        any::<bool>(),
        prop_oneof![
            "\\PC{0,3000}".prop_map(SummaryText::Inline),
            Just(SummaryText::Stored)
        ],
        prop::collection::vec("\\PC{0,40}", 0..3),
    )
        .prop_map(|(is_user, text, attachments)| SummaryLine {
            who: if is_user {
                SummaryWho::User
            } else {
                SummaryWho::Assistant {
                    name: "助手".into(),
                }
            },
            text,
            attachments,
        })
}

proptest! {
    /// 任意输入：不 panic；整份不超上限；开头固定；第一条用户消息与最新一条一定在。
    #[test]
    fn never_exceeds_limits(
        first in prop::option::of(arb_line()),
        earlier in prop::collection::vec(arb_line(), 0..30),
        unfetched in 0u32..1000,
    ) {
        let source = SummarySource { first_user: first.clone(), earlier: earlier.clone(), unfetched, request: None };
        let limits = SummaryLimits::DEFAULT;
        match resume_summary(&source, limits) {
            None => prop_assert!(first.is_none() && earlier.is_empty()),
            Some(out) => {
                prop_assert!(out.chars().count() <= limits.total());
                prop_assert!(out.starts_with(SUMMARY_HEADER));
                if let Some(f) = &first {
                    let head: String = entry(f, limits.per_line()).chars().take(20).collect();
                    prop_assert!(out.contains(&head));
                }
                if let Some(newest) = earlier.first() {
                    prop_assert!(out.ends_with(&entry(newest, limits.per_line())));
                }
            }
        }
    }
}
