//! 消息锚点的客户端一侧（T09 方案 2.9）：锚点窗口、当前位置、翻看与跳转。状态挂在打开的时间线上，
//! 换对话就整份丢掉。
//!
//! 哪些算锚点只在后台算（`ListAnchors`），这里不从已加载的行里挑（R13）；窗口按「以某处为中心前后各取
//! [`ANCHOR_SPAN`] 条」取，失效后重查、不推演，与侧栏摘要同一套（并进摘要重查的同一轮）。

use iwork_proto::pb;

/// 锚点窗口前后各取多少条。
pub const ANCHOR_SPAN: u32 = 60;
/// 跳到没加载的锚点时，以它为中心取多少条消息。
pub const JUMP_PAGE: u32 = 50;
/// 跳过去之后高亮多久（网页计时）。
pub const HIGHLIGHT_MS: u32 = 1000;
/// 以它为中心取锚点窗口就是整个对话最早的那一段（`around_seq` 取「不大于它的」与「大于它的」，seq 从 1 起）。
pub const EARLIEST_CENTER: u64 = 1;
/// 以它为中心就是最新的那一段（`ListAnchors.around_seq` 的约定）。
pub const LATEST_CENTER: u64 = 0;

/// 锚点的身份：`created_seq` 的十进制。三种锚点各自是不同的事件，seq 不会重。
pub fn key_of(a: &pb::Anchor) -> String {
    a.created_seq.to_string()
}

/// 当前位置（Q17 已定：可见区域第一行之上最近的一处）：`created_seq` 不大于第一行的 seq 的最后一处；
/// 第一行在第一处之前就取第一处。窗口为空返回 `None`。换口径只改这一个函数。
pub fn current_anchor(window: &[pb::Anchor], first_visible_seq: u64) -> Option<&pb::Anchor> {
    let n = window.partition_point(|a| a.created_seq <= first_visible_seq);
    window.get(n.saturating_sub(1)).or_else(|| window.first())
}

/// 这份窗口能不能确定阅读位置那一处：阅读位置在窗口第一处之前而前面还有，或在最后一处之后而后面还有，
/// 那一处就在窗口外面，确定不了（翻看换来的窗口常常这样）。
fn covers(w: &pb::ListAnchorsResult, reading: u64) -> bool {
    match (w.anchors.first(), w.anchors.last()) {
        (Some(first), Some(last)) => {
            !(reading < first.created_seq && w.has_before
                || reading > last.created_seq && w.has_after)
        }
        _ => !(w.has_before || w.has_after),
    }
}

/// 等着跳的那一处（目标不在时间线里，正在取以它为中心的一段）。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingJump {
    pub anchor: pb::Anchor,
}

/// 一次跳转的滚动指令：发出去一次，网页按 token 去重。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollTo {
    pub row_key: String,
    pub token: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Anchors {
    /// 最近一次取到的窗口；还没取到为 `None`（组件不显示）。
    pub window: Option<pb::ListAnchorsResult>,
    /// 这份窗口是以哪里为中心取的（0 = 最新）；摘要重查轮里按它重取。
    pub center: u64,
    /// 要以哪里为中心重取（0 = 最新）：当前位置出了窗口、翻看到边缘、相对跳转越界、走了快照。
    pub want_center: Option<u64>,
    /// 真实阅读位置：网页可见区域第一行的 seq。只随用户看的地方变，不随锚点窗口变（审查 4/F-7）。
    pub reading: Option<u64>,
    /// 当前位置（锚点的 key）：阅读位置之上最近的一处。只在窗口覆盖得到阅读位置时算；翻看换来的窗口
    /// 覆盖不到时保留原值，不拿浏览中心附近的某一处顶替。
    pub current: Option<String>,
    pub pending_jump: Option<PendingJump>,
    /// 相对跳转等窗口重取后再落（目标越过了窗口而那一侧还有）。
    pub pending_relative: Option<i32>,
    pub scroll: Option<ScrollTo>,
    next_token: u64,
}

/// 一次操作之后要做的事（会话层据此发查询、记脏）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnchorEffect {
    pub anchors_dirty: bool,
    pub cursor_dirty: bool,
    pub scroll_dirty: bool,
}

impl Anchors {
    /// 现在要不要单独取一份窗口、以哪里为中心：还没取过 → 最新（或已经想好的中心）；有想换的中心 → 它。
    /// 过时的重取不在这里：它进摘要重查的同一轮（[`crate::store::SummaryTable::Anchors`]），按 [`Anchors::center`] 取。
    pub fn wanted_center(&self) -> Option<u64> {
        match &self.window {
            None => Some(self.want_center.unwrap_or(0)),
            Some(_) => self.want_center,
        }
    }

    pub fn items(&self) -> &[pb::Anchor] {
        self.window.as_ref().map_or(&[], |w| w.anchors.as_slice())
    }

    pub fn find(&self, key: &str) -> Option<&pb::Anchor> {
        self.items().iter().find(|a| key_of(a) == key)
    }

    /// 新窗口到了：整份替换；以前预览的位置由网页按 key 对回去。
    pub fn replace_window(&mut self, center: u64, w: pb::ListAnchorsResult) -> AnchorEffect {
        self.window = Some(w);
        self.center = center;
        if self.want_center == Some(center) {
            self.want_center = None;
        }
        AnchorEffect {
            anchors_dirty: true,
            ..Default::default()
        }
    }

    /// 查询结果到了：换上窗口，再按可见位置（`first_visible_seq`，还没有可见行为 `None`）更新当前位置。
    /// 第一份窗口要完整核对一次（出了窗口就重取）：它到之前网页报的可见位置没有窗口可比，被跳过了。
    /// 之后的窗口只重算位置，见 [`Anchors::recompute_current`]。
    pub fn window_arrived(
        &mut self,
        center: u64,
        w: pb::ListAnchorsResult,
        first_visible_seq: Option<u64>,
    ) -> AnchorEffect {
        let first = self.window.is_none();
        let mut e = self.replace_window(center, w);
        if let Some(seq) = first_visible_seq {
            let follow = if first {
                self.on_visible(seq)
            } else {
                self.recompute_current(seq)
            };
            e.cursor_dirty |= follow.cursor_dirty;
        }
        e
    }

    /// 网页报了新的可见窗口（第一行的 seq）：阅读位置变了。重算当前位置；位置出了已取的窗口、那一侧还有，
    /// 就以它为中心重取，取到之前当前位置不确定（`None`）。
    pub fn on_visible(&mut self, first_visible_seq: u64) -> AnchorEffect {
        let moved = self.reading != Some(first_visible_seq);
        self.reading = Some(first_visible_seq);
        let Some(w) = &self.window else {
            return AnchorEffect::default();
        };
        if !covers(w, first_visible_seq) {
            self.want_center = Some(first_visible_seq);
            if moved && self.current.is_some() {
                self.current = None;
                return AnchorEffect {
                    cursor_dirty: true,
                    ..Default::default()
                };
            }
            return AnchorEffect::default();
        }
        self.recompute_current(first_visible_seq)
    }

    /// 只重算当前位置，不重取（锚点窗口刚换过：翻看出来的窗口可以不包含当前位置，不能又把它拉回去）。
    /// 窗口覆盖不到阅读位置时当前位置不动。
    pub fn recompute_current(&mut self, first_visible_seq: u64) -> AnchorEffect {
        self.reading = Some(first_visible_seq);
        let mut e = AnchorEffect::default();
        if !self
            .window
            .as_ref()
            .is_some_and(|w| covers(w, first_visible_seq))
        {
            return e;
        }
        let current = current_anchor(self.items(), first_visible_seq).map(key_of);
        if current != self.current {
            self.current = current;
            e.cursor_dirty = true;
        }
        e
    }

    /// Home / End：以整个对话最早或最新的一头为中心重取（不跳转）。
    pub fn browse_edge(&mut self, latest: bool) {
        self.want_center = Some(if latest {
            LATEST_CENTER
        } else {
            EARLIEST_CENTER
        });
    }

    /// 翻看到了离窗口边缘不远的一处：以它为中心重取（不跳转）。
    pub fn browse(&mut self, key: &str) {
        if let Some(a) = self.find(key) {
            self.want_center = Some(a.created_seq);
        }
    }

    /// 跳到某一处。`row` 是它在时间线里对应的行（调用方查好，见 `Timeline::anchor_row`）：在 → 发滚动指令；
    /// 不在 → 记下等着，返回真表示要取以它为中心的一段。
    pub fn jump(&mut self, a: pb::Anchor, row: Option<String>) -> (AnchorEffect, bool) {
        self.pending_relative = None;
        match row {
            Some(row) => {
                self.pending_jump = None;
                (self.emit_scroll(row), false)
            }
            None => {
                self.pending_jump = Some(PendingJump { anchor: a });
                (
                    AnchorEffect {
                        anchors_dirty: true,
                        ..Default::default()
                    },
                    true,
                )
            }
        }
    }

    /// 相对当前位置跳一处。目标在窗口里 → 返回它的 key（调用方照 [`Anchors::jump`] 跳）；
    /// 越过了窗口而那一侧还有 → 先以当前位置为中心重取，等窗口到了再落；到头了什么都不做。
    /// 当前位置不在这份窗口里（翻看换来的窗口）→ 围绕真实阅读位置重取再落（审查 4/F-7）。
    pub fn relative_target(&mut self, delta: i32) -> Option<String> {
        let w = self.window.as_ref()?;
        let items = &w.anchors;
        let Some(at) = self
            .current
            .as_deref()
            .and_then(|c| items.iter().position(|a| key_of(a) == c))
        else {
            if let Some(reading) = self.reading {
                self.pending_relative = Some(delta);
                self.want_center = Some(reading);
            }
            return None;
        };
        let target = i64::try_from(at).ok()? + i64::from(delta);
        if let Some(a) = usize::try_from(target).ok().and_then(|t| items.get(t)) {
            return Some(key_of(a));
        }
        let more = if delta < 0 { w.has_before } else { w.has_after };
        if more {
            self.pending_relative = Some(delta);
            self.want_center = items.get(at).map(|a| a.created_seq);
        }
        None
    }

    /// 以 `center` 为中心的那一段到了、时间线已经重建：等着的正是它，就落到目标行（`row` 是那一处现在对应的行），
    /// 或者（目标已经不在）只显示这一段、不高亮。等着的是别的（或没有在等），什么都不做（审查 4/F-5）。
    pub fn landed(&mut self, center: u64, row: Option<String>) -> AnchorEffect {
        if self
            .pending_jump
            .as_ref()
            .is_none_or(|j| j.anchor.created_seq != center)
        {
            return AnchorEffect::default();
        }
        self.pending_jump = None;
        let mut e = match row {
            Some(row) => self.emit_scroll(row),
            None => AnchorEffect::default(),
        };
        e.anchors_dirty = true;
        e
    }

    /// 用户自己滚动了：等着的跳转（居中加载、等窗口的相对跳转）都作废（审查 4/F-6）。
    pub fn cancel_waiting(&mut self) -> AnchorEffect {
        let had_jump = self.pending_jump.take().is_some();
        self.pending_relative = None;
        AnchorEffect {
            // 加载提示（AnchorsView.jump_loading）随之收掉
            anchors_dirty: had_jump,
            ..Default::default()
        }
    }

    /// 时间线因为快照重建成了最新页（打开对话、重连走了快照、Q9）：不再跳，锚点按最新位置重取。
    pub fn reset_to_latest(&mut self) -> AnchorEffect {
        let had_jump = self.pending_jump.take().is_some();
        self.pending_relative = None;
        self.want_center = Some(0);
        AnchorEffect {
            anchors_dirty: had_jump,
            ..Default::default()
        }
    }

    fn emit_scroll(&mut self, row_key: String) -> AnchorEffect {
        self.next_token += 1;
        self.scroll = Some(ScrollTo {
            row_key,
            token: self.next_token,
        });
        AnchorEffect {
            scroll_dirty: true,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(seq: u64) -> pb::Anchor {
        pb::Anchor {
            created_seq: seq,
            ..Default::default()
        }
    }

    #[test]
    fn current_anchor_table() {
        let w = [a(10), a(20), a(30)];
        let at = |first: u64| current_anchor(&w, first).map(|x| x.created_seq);
        assert_eq!(at(5), Some(10), "第一处之前：取第一处");
        assert_eq!(at(10), Some(10), "正好在一处上");
        assert_eq!(at(25), Some(20), "两处之间：之上最近的一处");
        assert_eq!(at(99), Some(30), "最后一处之后");
        assert_eq!(current_anchor(&[], 5), None, "窗口为空");
    }

    fn window(seqs: &[u64], before: bool, after: bool) -> pb::ListAnchorsResult {
        pb::ListAnchorsResult {
            anchors: seqs.iter().map(|s| a(*s)).collect(),
            has_before: before,
            has_after: after,
            ..Default::default()
        }
    }

    #[test]
    fn current_outside_window_refetches() {
        let mut s = Anchors::default();
        s.replace_window(0, window(&[100, 200], true, false));
        s.on_visible(150);
        assert_eq!(s.want_center, None, "在窗口里不重取");
        s.on_visible(50);
        assert_eq!(
            s.want_center,
            Some(50),
            "第一行在窗口之前、前面还有：以它为中心重取"
        );
        let mut t = Anchors::default();
        t.replace_window(0, window(&[100, 200], false, false));
        t.on_visible(50);
        assert_eq!(t.want_center, None, "对照：前面没有了就不重取");
    }

    /// 第一份窗口到达之前网页已经报过可见位置（打开对话就往上翻）：窗口一到就按那个位置核对，出了窗口就重取。
    /// 之后换来的窗口（翻看、重取）只重算位置，不把翻看出来的窗口拉回去。
    #[test]
    fn first_window_checks_visible_position() {
        let mut s = Anchors::default();
        s.window_arrived(0, window(&[100, 200], true, false), Some(50));
        assert_eq!(
            s.want_center,
            Some(50),
            "第一份窗口：位置在它之前、前面还有，重取"
        );

        let mut t = Anchors::default();
        t.window_arrived(0, window(&[100, 200], true, false), Some(150));
        assert_eq!(t.want_center, None, "对照：位置在窗口里不重取");
        t.window_arrived(300, window(&[300, 400], true, false), Some(150));
        assert_eq!(t.want_center, None, "后来的窗口（翻看来的）不拉回去");
        assert_eq!(
            t.current.as_deref(),
            Some("100"),
            "翻看来的窗口覆盖不到阅读位置（150 在它前面、前面还有）：当前位置不动（审查 4/F-7）"
        );
    }

    #[test]
    fn jump_to_loaded_row_emits_scroll_once_per_token() {
        let mut s = Anchors::default();
        s.replace_window(0, window(&[10, 20], false, false));
        let (e, fetch) = s.jump(a(20), Some("m20".into()));
        assert!(e.scroll_dirty && !fetch);
        let first = s.scroll.clone().unwrap();
        s.jump(a(20), Some("m20".into()));
        assert_ne!(
            s.scroll.clone().unwrap().token,
            first.token,
            "每次跳转换一个 token"
        );
        let (_, fetch) = s.jump(a(10), None);
        assert!(fetch, "目标不在时间线里：要取以它为中心的一段");
        assert_eq!(
            s.pending_jump.as_ref().map(|p| p.anchor.created_seq),
            Some(10)
        );
    }

    #[test]
    fn relative_at_window_edge_fetches_first() {
        let mut s = Anchors::default();
        s.replace_window(0, window(&[10, 20], true, false));
        s.on_visible(10);
        assert_eq!(s.relative_target(1).as_deref(), Some("20"));
        assert_eq!(s.relative_target(-1), None, "越过窗口前端");
        assert_eq!(s.pending_relative, Some(-1));
        assert_eq!(s.want_center, Some(10), "以当前位置为中心重取");
        let mut t = Anchors::default();
        t.replace_window(0, window(&[10, 20], false, false));
        t.on_visible(20);
        assert_eq!(t.relative_target(1), None, "到头了");
        assert_eq!(t.pending_relative, None, "对照：那一侧没有就不重取");
    }

    #[test]
    fn landed_without_target_shows_segment_only() {
        let mut s = Anchors::default();
        s.replace_window(0, window(&[10], false, false));
        s.jump(a(10), None);
        assert_eq!(
            s.landed(99, None),
            AnchorEffect::default(),
            "别的跳转的结果不消费这一次"
        );
        assert!(s.pending_jump.is_some());
        let e = s.landed(10, None);
        assert!(!e.scroll_dirty, "目标不在：不滚动、不高亮");
        assert_eq!(s.pending_jump, None);
        s.jump(a(10), None);
        assert!(s.landed(10, Some("m10".into())).scroll_dirty);
    }

    #[test]
    fn snapshot_reset_drops_pending_jump() {
        let mut s = Anchors::default();
        s.replace_window(0, window(&[10], false, false));
        s.jump(a(10), None);
        s.reset_to_latest();
        assert_eq!(s.pending_jump, None);
        assert_eq!(s.want_center, Some(0));
    }
}
