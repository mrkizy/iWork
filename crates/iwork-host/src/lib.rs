//! 基础设施层：本机能力——路径事实（`PathResolver`）、亲手执行卡片的用户执行（`ExecutionBackend`）
//! 与系统钥匙串（`SecretStore`，T10 填实现），以及项目身份文件与新建项目目录（[`project_files`]，T08）。

#[cfg(unix)]
pub mod autostart;
pub mod fs;
pub mod project_files;
