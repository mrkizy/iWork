//! 求值用到的环境事实（T07 方案 2.2）：由 daemon 组装后传进来，核心不读环境变量、不碰磁盘。

use iwork_domain::action::CanonicalPath;
use iwork_domain::id::{AssistantId, ConversationId, ProjectId};

/// 平台：内置规则集里的路径按平台取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    MacOs,
    Linux,
    Windows,
}

/// 一条会话的路径事实。全部是规范路径（daemon 用 `PathResolver` 或 `canonicalize` 算好）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvPaths {
    pub platform: Platform,
    pub home: CanonicalPath,
    /// iWork 数据目录（数据库、制品、附件区、导入区）
    pub data_dir: CanonicalPath,
    /// 用户可见的 iWork 根目录（`~/iWork`），下有 `projects/`、`assistants/`
    pub iwork_root: CanonicalPath,
    /// 本会话的工作区：项目根目录；私聊选「私聊」时是助手的私有目录
    pub workspace_root: CanonicalPath,
    /// 系统临时目录（可写）
    pub tmp_dirs: Vec<CanonicalPath>,
    /// 私聊时助手的私有目录（可写）；项目对话为 `None`
    pub dm_private_dir: Option<CanonicalPath>,
    /// iWork 登记的其他项目目录：会话快照里转成临时敏感目录条目（需求第 9 章）
    pub other_project_roots: Vec<CanonicalPath>,
    /// 其他助手的私聊目录：同上
    pub other_dm_dirs: Vec<CanonicalPath>,
}

impl EnvPaths {
    pub fn home_join(&self, rel: &str) -> CanonicalPath {
        CanonicalPath(self.home.0.join(rel))
    }

    pub fn data_join(&self, rel: &str) -> CanonicalPath {
        CanonicalPath(self.data_dir.0.join(rel))
    }

    pub fn workspace_join(&self, rel: &str) -> CanonicalPath {
        CanonicalPath(self.workspace_root.0.join(rel))
    }
}

/// 规则范围过滤用的键（方案 4.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopeKeys {
    /// 私聊选「私聊」时为 `None`：项目级条目都不生效
    pub project: Option<ProjectId>,
    pub assistant: AssistantId,
    pub conversation: ConversationId,
}
