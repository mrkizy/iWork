//! 审批求值的类型（方案 4.4）：判定结果、规则条目、规则集。只定类型，求值在 T07（`iwork-core`）。

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::action::{ActionFingerprint, ActionFlag, ActionKind};
use crate::approval_mode::ApprovalMode;
use crate::common::{Actor, Timestamp};
use crate::id::{ApprovalId, AssistantId, ConversationId, ProjectId, RuleId};
use crate::user_action::Proposal;

/// 判定结果固定五种（需求第 9 章）；新增须产品负责人决定。
///
/// 没有「记住」：记住是 `DecideApproval` 的参数，不是判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// 询问用户（自动档下越界类先经 AI 审查）
    Ask(AskReason),
    /// 含底线、拒绝记忆、工具配置禁用、拒绝规则、系统回合越界
    Deny(DenyReason),
    /// 拒绝 + 亲手执行提议（底线命中且是单个简单命令或补丁）
    UserAction(Proposal),
    /// 询问的一种：批准后在沙箱外运行
    OutsideSandbox(AskReason),
}

/// 为什么要问。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AskReason {
    Default,
    OutOfBounds,
    MustAsk,
    Unclassified,
    McpNotReadOnly,
    UnparsableCommand,
    /// 用户拒绝过同一动作后，本该放行的也改为询问
    ForcedAfterDenial,
}

/// 为什么拒绝。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DenyReason {
    Baseline(RuleId),
    DenialMemory(ActionFingerprint),
    ToolDisabled,
    DenyRule(RuleId),
    SystemTurnOutOfToolSet,
    ToolSetExcluded,
}

/// 规则所在的层。安全清单的三张清单也是规则条目，属于哪张清单由层决定（技术设计 7.2c，见 [`crate::security_list`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleLayer {
    Baseline,
    DenialMemory,
    ToolConfig,
    UserDeny,
    MustAsk,
    Remembered,
    ProjectAllow,
    Default,
    /// 安全清单：需凭据命令（值在 `ProgramSubcommand` 里）
    CredentialCommands,
    AutorunConfig,
    /// 安全清单：敏感目录（值在 `PathUnder` 里）
    SensitiveDirs,
    WritableCache,
    /// 安全清单：可写目录（值在 `PathUnder` 里）
    WritableDirs,
    /// 安全清单的拒收名单
    RejectList,
}

/// 规则的匹配条件。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RuleMatcher {
    PathUnder(PathBuf),
    PathIs(PathBuf),
    ProgramSubcommand {
        program: String,
        subcommand: Option<String>,
    },
    ToolName(String),
    ActionKindIs(ActionKind),
    Any,
    /// 全部子条件同时命中（T07 Q1）。空列表恒命中，与 `Any` 同义
    All(Vec<RuleMatcher>),
    /// 动作带这个标记（越界、清单外重试、执行脚本）
    Flag(ActionFlag),
}

/// 规则的效果。比 [`Verdict`] 多一个 `UserActionEligible`：底线条目只说「拒绝，且允许生成卡片」，
/// 卡片内容由求值时的动作生成。没有任何取值能把底线变成放行——「不可提权」在类型上成立。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleEffect {
    Allow,
    Ask,
    Deny,
    UserActionEligible,
    OutsideSandbox,
}

/// 规则从哪来。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RuleSource {
    Builtin { ruleset_version: RuleSetVersion },
    Remembered { approval_id: ApprovalId, by: Actor },
    ProjectConfig,
    User,
}

/// 规则或清单条目的生效范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleScope {
    Global,
    Project(ProjectId),
    Assistant(AssistantId),
    Conversation(ConversationId),
}

/// 内置规则集的版本号，随代码发布；审计记录命中规则时一并记下（技术设计 7.4）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RuleSetVersion(pub String);

/// 条目在哪些档位生效。空集 = 全部档位（旧事件没有这个字段，解出来就是空集）。
pub type ApprovalModes = BTreeSet<ApprovalMode>;

/// 规则条目：安全规则都是这一种数据（需求第 9 章）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEntry {
    pub id: RuleId,
    pub layer: RuleLayer,
    pub matcher: RuleMatcher,
    pub effect: RuleEffect,
    pub source: RuleSource,
    pub scope: RuleScope,
    pub created_at: Timestamp,
    /// 给人看的说明，如「本项目内 `npm test …` 不再问」
    pub label: String,
    /// 只在这些档位生效；空集 = 全部档位（T07 Q1）。必问层的删除只在手动、自动档生效就靠它
    pub applies_in: ApprovalModes,
}

impl RuleEntry {
    /// 这条条目在 `mode` 档位下是否生效。
    pub fn applies_in_mode(&self, mode: ApprovalMode) -> bool {
        self.applies_in.is_empty() || self.applies_in.contains(&mode)
    }
}

/// 求值用的规则快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSet {
    pub version: RuleSetVersion,
    pub entries: Vec<RuleEntry>,
}

/// 一次求值的结果，写进审计事件 `ApprovalEvaluated`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    pub verdict: Verdict,
    pub matched: Vec<RuleId>,
    pub layer_hit: RuleLayer,
    pub ruleset_version: RuleSetVersion,
}
