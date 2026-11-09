//! 项目（技术设计 8.2、9.3、9.6）。

use std::path::PathBuf;

use crate::approval_mode::ProjectDefaultMode;
use crate::common::{ModelSelection, Timestamp};
use crate::error::DomainError;
use crate::id::{ProjectId, RuntimeId};
use crate::machine::{IllegalTransition, Transition, finite_enum, lookup};

/// 版本控制种类：不透明字符串，具体名字只在 VcsProvider 实现里出现。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VcsKind(pub String);

finite_enum! {
    /// 项目路径状态。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum PathStatus {
        Ok,
        /// 界面显示「找不到路径」
        Missing,
    }
}

finite_enum! {
    /// 项目路径的触发。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum PathTrigger {
        /// 启动或打开项目时发现路径不在了
        PathLost,
        /// 再检查时原路径又出现了
        PathFound,
        /// 用户选了新位置且项目 ID 一致（`RelocateProject`）
        Relocated,
    }
}

use PathStatus as S;
use PathTrigger as T;

/// 项目路径转换表（方案 4.3）。
pub const PATH_TRANSITIONS: &[Transition<PathStatus, PathTrigger>] = &[
    (S::Ok, T::PathLost, S::Missing),
    (S::Missing, T::PathFound, S::Ok),
    (S::Missing, T::Relocated, S::Ok),
];

pub const PATH_MACHINE: &str = "project_path";

pub type PathIllegal = IllegalTransition<PathStatus, PathTrigger>;

/// 按 [`PATH_TRANSITIONS`] 求下一状态。
pub fn next(status: PathStatus, trigger: PathTrigger) -> Result<PathStatus, PathIllegal> {
    lookup(PATH_MACHINE, PATH_TRANSITIONS, status, trigger)
}

/// 默认 AI 审查超时（技术设计 7.2：超过 60 秒未给出结果转为询问用户）。
pub const DEFAULT_AI_REVIEW_TIMEOUT_SECS: u32 = 60;

/// AI 审查设置（技术设计 7.2、9.6 第 8 条）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiReviewSettings {
    pub enabled: bool,
    /// `None` 表示与 iWork 相同
    pub runtime_id: Option<RuntimeId>,
    /// `None` 表示与 iWork 相同
    pub model: Option<ModelSelection>,
    pub timeout_secs: u32,
}

impl Default for AiReviewSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            runtime_id: None,
            model: None,
            timeout_secs: DEFAULT_AI_REVIEW_TIMEOUT_SECS,
        }
    }
}

/// 项目设置。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectSettings {
    pub default_approval_mode: ProjectDefaultMode,
    /// `None` 表示用助手自己的默认模型
    pub default_model: Option<ModelSelection>,
    pub ai_review: AiReviewSettings,
}

/// 项目实体。以 `id`（也写在仓库的 `.iwork/project.toml`）识别，不以路径识别。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: ProjectId,
    pub root_path: PathBuf,
    pub vcs_kind: VcsKind,
    pub name: String,
    pub pinned: bool,
    pub last_active_at: Option<Timestamp>,
    pub path_status: PathStatus,
    pub settings: ProjectSettings,
}

impl Project {
    /// 重新定位：新位置读出的项目 ID 一致才关联，否则返回 `ProjectIdMismatch` 且什么都不改。
    pub fn relocate(&mut self, new_path: PathBuf, found_id: ProjectId) -> Result<(), DomainError> {
        if found_id != self.id {
            return Err(DomainError::ProjectIdMismatch {
                expected: self.id,
                found: found_id,
            });
        }
        self.path_status = next(self.path_status, T::Relocated)?;
        self.root_path = new_path;
        Ok(())
    }
}
