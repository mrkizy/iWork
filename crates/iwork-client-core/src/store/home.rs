//! 首页（画布 01）：还没有对话时的输入框。
//!
//! 「运行时 · 模型」与档位在首页就能选（ui-m1.md 第 2 节第 9、14 条），可那时还没有对话可设：选的值记在这里，
//! 发出首条消息时随新对话一起设上。「新建对话 → 设上模型与档位 → 发出首条消息 → 跳过去」整个过程也记在这里，
//! 由会话层（session::start）一步步推进；网页只发一次 StartConversation，等跳转或错误。

use iwork_proto::pb;

#[derive(Debug, Default)]
pub struct Home {
    /// 首页选过的模型（只对下一个新建的对话）；没选过用助手配置的。
    pub model: Option<pb::ModelSelection>,
    /// 首页选过的档位；没选过用项目默认。
    pub mode: Option<pb::ApprovalMode>,
    /// 正在进行的一次「新建并发出」；同一时间只有一次。
    pub start: Option<Start>,
    /// 上一次没能发出去的原因；下一次发出时清掉。
    pub error: Option<StartError>,
}

/// 没能发出去的原因（文字在 derive::text）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// 新建对话被拒（后台给的原因）。
    Create(String),
    /// 对话建好了，设模型、档位或发消息被拒（后台给的原因）。
    Send(String),
    /// 断线重连时来的是快照，认不出哪个是新建的对话。
    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Start {
    pub project_id: String,
    pub text: String,
    /// 新对话建好、配上之后才有。
    pub conversation_id: Option<String>,
}

impl Home {
    /// 开始一次：清掉上一次的错误。
    pub fn begin(&mut self, project_id: String, text: String) {
        self.error = None;
        self.start = Some(Start {
            project_id,
            text,
            conversation_id: None,
        });
    }

    /// 发出去了：这一次的选择用完，下一次新建从默认开始。
    pub fn finish(&mut self) -> Option<Start> {
        self.model = None;
        self.mode = None;
        self.start.take()
    }

    /// 没发出去：留下原因，选择保留（用户改一改再发）。
    pub fn fail(&mut self, why: StartError) {
        self.start = None;
        self.error = Some(why);
    }
}
