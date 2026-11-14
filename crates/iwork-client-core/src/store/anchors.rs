//! 消息锚点的客户端一侧（T09 方案 2.9）：锚点窗口、当前位置、翻看与跳转。状态挂在打开的时间线上，
//! 换对话就整份丢掉。
//!
//! 哪些算锚点只在后台算（`ListAnchors`），这里不从已加载的行里挑（R13）；窗口按「以某处为中心前后各取
//! [`ANCHOR_SPAN`] 条」取，失效后重查、不推演，与侧栏摘要同一套（并进摘要重查的同一轮）。
//!
//! ## 一次性滚动意图（只在这里持有，网页只执行带 token 的指令）
//!
//! 两类，各最多一个，新的替换旧的：
//! - 跳到某一行（[`Anchors::row`]，一个 [`RowIntent`]）：点锚点、⌥↑ / ⌥↓、从「待你处理」进来。所有入口都经
//!   `set_row` 替换或作废它（跳转、相对跳转、回到最新、用户滚动、快照、换代），没有平行的字段。
//! - 锚点列表的头尾（[`Anchors::edge`]）：Home / End。
//!
//! | 阶段 | 等什么 | 到了做什么 |
//! | --- | --- | --- |
//! | 等底 | 对话的首份快照（[`Anchors::base_loaded`] 为假） | 目标行在时间线里就发滚动；不在就转「等数据」 |
//! | 等数据 | 以目标为中心的那一段（AroundPage）/ 以那一头为中心的锚点窗口 | 对得上才发：滚动指令 / 头尾落点 |
//! | 已发出 | 网页按 token 用掉（同一个只用一次；全量帧不重发） | — |
//!
//! 作废：用户自己滚动时间线（跳行，[`Anchors::cancel_waiting`]）；用户在锚点列表里滚动或按键（头尾，
//! [`Anchors::cancel_list_input`]）；首份快照之后的快照（重连走快照、Q9，[`Anchors::reset_to_latest`]）；网页
//! attach 换代（[`Anchors::on_attach`]）；换对话（整份丢）。作废以后迟到的结果照常进时间线、换窗口，但不再执行。

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

/// 跳行意图（同一时刻最多一个）。未发出的滚动指令也挂在这里（[`RowIntent::Issued`]）：被替换或作废时一并丢掉，
/// 不会再随下一帧发出去。
#[derive(Debug, Clone, Default, PartialEq)]
pub enum RowIntent {
    #[default]
    None,
    /// 等底：对话的首份快照还没到。
    WaitBase(pb::Anchor),
    /// 等以目标为中心的那一段（AroundPage，`created_seq` 为中心）。
    WaitSegment(pb::Anchor),
    /// 相对跳转（⌥↑ / ⌥↓）等以 `center` 为中心的锚点窗口，到了再按 `delta` 落。
    WaitWindow { delta: i32, center: u64 },
    /// 已产生滚动指令；网页按 token 只用一次，全量帧不重发。
    Issued(ScrollTo),
}

impl RowIntent {
    /// 正在跳的那一处（等底、等那一段时；网页据此画流光与进度线）。
    pub fn loading(&self) -> Option<&pb::Anchor> {
        match self {
            RowIntent::WaitBase(a) | RowIntent::WaitSegment(a) => Some(a),
            _ => None,
        }
    }

    /// 在等以哪个 seq 为中心的那一段（会话层据此发 AroundPage）。
    pub fn segment_seq(&self) -> Option<u64> {
        match self {
            RowIntent::WaitSegment(a) => Some(a.created_seq),
            _ => None,
        }
    }

    /// 已产生的滚动指令。
    pub fn scroll(&self) -> Option<&ScrollTo> {
        match self {
            RowIntent::Issued(s) => Some(s),
            _ => None,
        }
    }
}

/// 一次跳转的滚动指令：发出去一次，网页按 token 去重。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollTo {
    pub row_key: String,
    pub token: u64,
}

/// 滚动指令与头尾落点的 token：整个进程单调递增，换对话、换代次都不回头，网页按「没见过」去重才可靠。
fn next_token() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// 以它为中心取的锚点窗口就是那一头。
fn edge_center(latest: bool) -> u64 {
    if latest {
        LATEST_CENTER
    } else {
        EARLIEST_CENTER
    }
}

/// 锚点列表的 Home / End。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// 在等以那一头为中心的锚点窗口。
    Waiting { latest: bool },
    /// 那一头到了：网页按 token 把预览移过去（只用一次）。
    Landed { latest: bool, token: u64 },
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
    /// 跳行意图：只经 `set_row` 改。
    pub row: RowIntent,
    /// 对话的首份快照到了没有。没到之前的跳转只记下（「等底」），到了再定：首份快照是这次打开的底，不作废；
    /// 之后的快照（重连、Q9）才作废。
    pub base_loaded: bool,
    pub edge: Option<Edge>,
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

    /// 新窗口到了：整份替换；以前预览的位置由网页按 key 对回去。等着的头尾正是这一头的就落点。
    pub fn replace_window(&mut self, center: u64, w: pb::ListAnchorsResult) -> AnchorEffect {
        self.window = Some(w);
        self.center = center;
        if self.want_center == Some(center) {
            self.want_center = None;
        }
        if let Some(Edge::Waiting { latest }) = self.edge
            && edge_center(latest) == center
        {
            self.edge = Some(Edge::Landed {
                latest,
                token: next_token(),
            });
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

    /// Home / End：以整个对话最早或最新的一头为中心重取（不跳转），窗口到了给网页一个头尾落点。
    pub fn browse_edge(&mut self, latest: bool) -> AnchorEffect {
        self.want_center = Some(edge_center(latest));
        self.edge = Some(Edge::Waiting { latest });
        AnchorEffect {
            anchors_dirty: true,
            ..Default::default()
        }
    }

    /// 用户在锚点列表里滚动或按键：头尾意图作废（迟到的窗口照常换上，但不再把预览拉到那一头）。
    pub fn cancel_list_input(&mut self) -> AnchorEffect {
        AnchorEffect {
            anchors_dirty: self.edge.take().is_some(),
            ..Default::default()
        }
    }

    /// 对话的底到了（首份快照已进时间线）：等底的跳转现在定。`row` 是它在时间线里对应的行（调用方查好）：
    /// 在就发滚动；不在就留着等以它为中心的那一段（会话层这时才发 AroundPage）。
    pub fn base_arrived(&mut self, row: Option<String>) -> AnchorEffect {
        self.base_loaded = true;
        let RowIntent::WaitBase(a) = &self.row else {
            return AnchorEffect::default();
        };
        match row {
            Some(row) => self.issue(row),
            None => {
                let a = a.clone();
                self.set_row(RowIntent::WaitSegment(a))
            }
        }
    }

    /// 网页重新 attach（换代次，如重载）：一次性意图全部作废。没发出的滚动指令不再发，迟到的结果只进时间线。
    pub fn on_attach(&mut self) {
        self.set_row(RowIntent::None);
        self.edge = None;
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
        if !self.base_loaded {
            // 等底：首份快照还没到，先记下，到了由 base_arrived 定
            return (self.set_row(RowIntent::WaitBase(a)), false);
        }
        match row {
            Some(row) => (self.issue(row), false),
            None => (self.set_row(RowIntent::WaitSegment(a)), true),
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
                self.set_row(RowIntent::WaitWindow {
                    delta,
                    center: reading,
                });
                self.want_center = Some(reading);
            }
            return None;
        };
        let target = i64::try_from(at).ok()? + i64::from(delta);
        if let Some(a) = usize::try_from(target).ok().and_then(|t| items.get(t)) {
            return Some(key_of(a));
        }
        let more = if delta < 0 { w.has_before } else { w.has_after };
        if more && let Some(center) = items.get(at).map(|a| a.created_seq) {
            self.set_row(RowIntent::WaitWindow { delta, center });
            self.want_center = Some(center);
        }
        None
    }

    /// 以 `center` 为中心的锚点窗口到了：等着它的相对跳转现在落，交回 `delta`（调用方照 ⌥↑ / ⌥↓ 再算一次）。
    /// 别的中心的窗口不消费它。
    pub fn take_relative(&mut self, center: u64) -> Option<i32> {
        match self.row {
            RowIntent::WaitWindow { delta, center: c } if c == center => {
                self.set_row(RowIntent::None);
                Some(delta)
            }
            _ => None,
        }
    }

    /// 以 `center` 为中心的那一段到了、时间线已经重建：等着的正是它，就落到目标行（`row` 是那一处现在对应的行），
    /// 或者（目标已经不在）只显示这一段、不高亮。等着的是别的（或没有在等），什么都不做（审查 4/F-5）。
    pub fn landed(&mut self, center: u64, row: Option<String>) -> AnchorEffect {
        if self.row.segment_seq() != Some(center) {
            return AnchorEffect::default();
        }
        match row {
            Some(row) => self.issue(row),
            None => self.set_row(RowIntent::None),
        }
    }

    /// 跳行意图作废（不管在哪个阶段，含已产生还没发出的指令）：用户自己滚动（审查 4/F-6）、回到最新。
    pub fn cancel_row(&mut self) -> AnchorEffect {
        self.set_row(RowIntent::None)
    }

    /// 首份快照之后又走了快照（重连、Q9），时间线回到最新页：一次性意图全部作废，锚点按最新位置重取。
    /// 首份快照不走这里（见 [`Anchors::base_arrived`]）。
    pub fn reset_to_latest(&mut self) -> AnchorEffect {
        let mut e = self.set_row(RowIntent::None);
        e.anchors_dirty |= self.edge.take().is_some();
        self.want_center = Some(LATEST_CENTER);
        e
    }

    /// 跳行意图的唯一写入口：换成 `next`（新的替换旧的，或作废成 None）。正在跳的那一处变了，锚点视图要重发
    /// （流光与进度线）；产生了指令就要发滚动。旧的未发出指令随之丢掉：下一帧按当前状态派生，没有就不发。
    fn set_row(&mut self, next: RowIntent) -> AnchorEffect {
        let loading_before = self.row.loading().map(key_of);
        self.row = next;
        AnchorEffect {
            anchors_dirty: loading_before != self.row.loading().map(key_of),
            scroll_dirty: matches!(self.row, RowIntent::Issued(_)),
            ..Default::default()
        }
    }

    /// 滚动指令的唯一产生处（Rust 侧没有第二处构造 ScrollTo）。
    fn issue(&mut self, row_key: String) -> AnchorEffect {
        self.set_row(RowIntent::Issued(ScrollTo {
            row_key,
            token: next_token(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 首份快照已经到了的锚点状态（这里的单元测试都在「有底」之后）。
    fn based() -> Anchors {
        Anchors {
            base_loaded: true,
            ..Anchors::default()
        }
    }

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
        let mut s = based();
        s.replace_window(0, window(&[100, 200], true, false));
        s.on_visible(150);
        assert_eq!(s.want_center, None, "在窗口里不重取");
        s.on_visible(50);
        assert_eq!(
            s.want_center,
            Some(50),
            "第一行在窗口之前、前面还有：以它为中心重取"
        );
        let mut t = based();
        t.replace_window(0, window(&[100, 200], false, false));
        t.on_visible(50);
        assert_eq!(t.want_center, None, "对照：前面没有了就不重取");
    }

    /// 第一份窗口到达之前网页已经报过可见位置（打开对话就往上翻）：窗口一到就按那个位置核对，出了窗口就重取。
    /// 之后换来的窗口（翻看、重取）只重算位置，不把翻看出来的窗口拉回去。
    #[test]
    fn first_window_checks_visible_position() {
        let mut s = based();
        s.window_arrived(0, window(&[100, 200], true, false), Some(50));
        assert_eq!(
            s.want_center,
            Some(50),
            "第一份窗口：位置在它之前、前面还有，重取"
        );

        let mut t = based();
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
        let mut s = based();
        s.replace_window(0, window(&[10, 20], false, false));
        let (e, fetch) = s.jump(a(20), Some("m20".into()));
        assert!(e.scroll_dirty && !fetch);
        let first = s.row.scroll().cloned().unwrap();
        s.jump(a(20), Some("m20".into()));
        assert_ne!(
            s.row.scroll().unwrap().token,
            first.token,
            "每次跳转换一个 token"
        );
        let (_, fetch) = s.jump(a(10), None);
        assert!(fetch, "目标不在时间线里：要取以它为中心的一段");
        assert_eq!(s.row.segment_seq(), Some(10));
    }

    #[test]
    fn relative_at_window_edge_fetches_first() {
        let mut s = based();
        s.replace_window(0, window(&[10, 20], true, false));
        s.on_visible(10);
        assert_eq!(s.relative_target(1).as_deref(), Some("20"));
        assert_eq!(s.relative_target(-1), None, "越过窗口前端");
        assert_eq!(
            s.row,
            RowIntent::WaitWindow {
                delta: -1,
                center: 10
            }
        );
        assert_eq!(s.want_center, Some(10), "以当前位置为中心重取");
        let mut t = based();
        t.replace_window(0, window(&[10, 20], false, false));
        t.on_visible(20);
        assert_eq!(t.relative_target(1), None, "到头了");
        assert_eq!(t.row, RowIntent::None, "对照：那一侧没有就不重取");
    }

    #[test]
    fn landed_without_target_shows_segment_only() {
        let mut s = based();
        s.replace_window(0, window(&[10], false, false));
        s.jump(a(10), None);
        assert_eq!(
            s.landed(99, None),
            AnchorEffect::default(),
            "别的跳转的结果不消费这一次"
        );
        assert_eq!(s.row.segment_seq(), Some(10));
        let e = s.landed(10, None);
        assert!(!e.scroll_dirty, "目标不在：不滚动、不高亮");
        assert_eq!(s.row, RowIntent::None);
        s.jump(a(10), None);
        assert!(s.landed(10, Some("m10".into())).scroll_dirty);
    }

    #[test]
    fn snapshot_reset_drops_pending_jump() {
        let mut s = based();
        s.replace_window(0, window(&[10], false, false));
        s.jump(a(10), None);
        s.reset_to_latest();
        assert_eq!(s.row, RowIntent::None);
        assert_eq!(s.want_center, Some(0));
    }
}
