//! 网页这一侧的最小模型：按方案 2.4 第 8 条核代次、按 (frame, index) 去重、收齐 last 再解码，
//! 再按 view.proto 的约定把 patch 折叠成「网页 store」。只为测试 Rust 侧交出去的东西能拼回全量。

#![allow(dead_code, reason = "各测试文件用其中一部分")]

use std::collections::BTreeMap;

use iwork_proto::pb;
use prost::Message;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct WebState {
    pub sidebar: Option<pb::SidebarView>,
    pub pending: Option<pb::PendingView>,
    pub settings: Option<pb::SettingsView>,
    pub assistant: Option<pb::AssistantProfileView>,
    pub add_project: Option<pb::AddProjectView>,
    pub connection: Option<pb::ConnectionView>,
    pub list: Option<pb::ConversationListView>,
    pub conversation_id: Option<String>,
    pub header: Option<pb::ConversationHeader>,
    pub composer: Option<pb::ComposerView>,
    pub window: Option<pb::WindowInfo>,
    pub notice: Option<String>,
    pub rows: BTreeMap<String, pb::RowView>,
    pub navigate: Option<pb::NavigatePatch>,
}

#[derive(Debug, Default)]
pub struct Web {
    pub epoch: u32,
    pub applied: Option<(u64, u32)>,
    buf: Vec<u8>,
    pub state: WebState,
    pub frames: u64,
    pub accepted_bytes: u64,
}

impl Web {
    pub fn new(epoch: u32) -> Self {
        Self {
            epoch,
            ..Default::default()
        }
    }

    pub fn request(&self) -> pb::NextFrame {
        pb::NextFrame {
            epoch: self.epoch,
            applied: self
                .applied
                .map(|(frame, index)| pb::FramePosition { frame, index }),
        }
    }

    /// 收一块。代次不对整个丢；重复的块丢；收齐 last 解码并应用。返回是否应用了一帧。
    pub fn accept(&mut self, c: &pb::FrameChunk) -> bool {
        if c.epoch != self.epoch {
            return false;
        }
        if self.applied == Some((c.frame, c.index)) {
            return false;
        }
        if c.index == 0 {
            self.buf.clear();
        }
        self.buf.extend_from_slice(&c.data);
        self.accepted_bytes += c.data.len() as u64;
        self.applied = Some((c.frame, c.index));
        if !c.last {
            return false;
        }
        let patch = pb::ViewPatch::decode(self.buf.as_slice()).expect("收齐的块能解码");
        self.buf.clear();
        self.apply(&patch);
        self.frames += 1;
        true
    }

    pub fn apply(&mut self, patch: &pb::ViewPatch) {
        use pb::view_change::Change as C;
        let s = &mut self.state;
        for ch in &patch.changes {
            match ch.change.as_ref().expect("变化非空") {
                C::Sidebar(p) => s.sidebar.clone_from(&p.view),
                C::Pending(p) => s.pending.clone_from(&p.view),
                C::Settings(p) => s.settings.clone_from(&p.view),
                C::Assistant(p) => s.assistant.clone_from(&p.view),
                C::AddProject(p) => s.add_project.clone_from(&p.view),
                C::Connection(p) => s.connection.clone_from(&p.view),
                C::ConversationList(p) => s.list.clone_from(&p.view),
                C::Navigate(p) => s.navigate = Some(p.clone()),
                C::Conversation(p) => {
                    if p.reset || s.conversation_id.as_deref() != Some(p.conversation_id.as_str()) {
                        s.rows.clear();
                        s.conversation_id = Some(p.conversation_id.clone());
                    }
                    for id in &p.removed_ids {
                        s.rows.remove(id);
                    }
                    for r in &p.rows {
                        s.rows.insert(r.key.clone(), r.clone());
                    }
                    for a in &p.appends {
                        let row = s.rows.get_mut(&a.message_id).expect("追加的行网页已有");
                        let Some(pb::row_view::Row::Message(m)) = row.row.as_mut() else {
                            panic!("追加只给消息行")
                        };
                        assert_eq!(m.text.len() as u64, a.offset, "追加前长度对得上");
                        m.text.push_str(&a.text);
                    }
                    s.header.clone_from(&p.header);
                    s.composer.clone_from(&p.composer);
                    s.window.clone_from(&p.window);
                    s.notice.clone_from(&p.blocking_notice);
                }
            }
        }
    }

    /// 按位置排好的行身份。
    pub fn row_keys(&self) -> Vec<String> {
        let mut v: Vec<_> = self
            .state
            .rows
            .values()
            .map(|r| (r.ordinal, r.key.clone()))
            .collect();
        v.sort();
        v.into_iter().map(|(_, k)| k).collect()
    }
}
