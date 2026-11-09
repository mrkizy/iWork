//! 全局设置（技术设计 4、9.3、9.7）。

use std::path::PathBuf;

/// 全局设置。项目设置在 [`crate::project::ProjectSettings`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalSettings {
    pub playful_ui: bool,
    pub autostart: bool,
    pub projects_root: PathBuf,
}
