//! 添加项目弹窗（画布 07；T09 方案 15.2 的「视图」与 `derive::add_project` 一张表）：候选目录的检查结果 → 提示框、
//! 初始化按钮、AGENTS.md 一行、生成草稿的勾选框、主按钮。路径一律经 `display_path` 写成 `~`，绝对路径不出 Rust。

use iwork_proto::pb;

use super::text;
use crate::store::Store;
use crate::store::add_project::{AddProjectDraft, Busy, Candidate, Check};

pub fn add_project(store: &Store) -> pb::AddProjectView {
    let home = store.home_dir.as_deref();
    let d = &store.add_project;
    pb::AddProjectView {
        clone: store.clone_job.as_ref().map(|j| pb::CloneProgressView {
            job_id: j.progress.job_id.clone(),
            phase: j.progress.phase.clone(),
            percent: j.progress.percent,
            text: j.progress.text.clone(),
            can_retry: j.error.is_some(),
            error: j.error.clone(),
        }),
        drafts: store
            .summary
            .pending
            .iter()
            .filter_map(|p| match &p.item {
                Some(pb::pending_item::Item::AgentsMdDraft(d)) => Some(pb::DraftCardView {
                    card_id: d.card_id.clone(),
                    project_id: d.project_id.clone(),
                    project_name: store
                        .summary
                        .project(&d.project_id)
                        .map_or_else(String::new, |p| p.name.clone()),
                }),
                _ => None,
            })
            .collect(),
        candidate: d.candidate.as_ref().map(|c| candidate(c, d, home)),
        parent_label: text::display_path(
            d.parent_dir.as_deref().unwrap_or(
                store
                    .settings
                    .as_ref()
                    .map_or("", |s| s.projects_root.as_str()),
            ),
            home,
        ),
        error: d.error.clone(),
        runtime_label: runtime_label(store),
        busy: d.busy.is_some(),
    }
}

/// 「Claude Code（全局默认）」：新项目用内置 iWork 的运行时（需求 FR19）。
fn runtime_label(store: &Store) -> String {
    let runtime = crate::derive::sidebar::new_assistant_runtime(store)
        .and_then(|id| store.runtimes.get(id))
        .map(|r| r.display_name.as_str());
    runtime.map_or_else(String::new, |n| {
        format!("{n}{}", text::RUNTIME_GLOBAL_DEFAULT)
    })
}

/// 方案 15.2 的表：检查中 / 没有版本控制 / 能加 / 已知 / 失败。
pub fn candidate(c: &Candidate, d: &AddProjectDraft, home: Option<&str>) -> pb::CandidateView {
    let mut v = pb::CandidateView {
        path_label: text::display_path(&c.path, home),
        ..Default::default()
    };
    match &c.check {
        Check::Checking => v.checking = true,
        Check::Failed(why) => {
            v.notice_title = text::ADD_CHECK_FAILED_TITLE.into();
            v.notice_body.clone_from(why);
            v.notice_tone = pb::Tone::Warning.into();
        }
        Check::Done(r) => {
            v.path_label = text::display_path(&r.root_path, home);
            let state = pb::ProjectPathState::try_from(r.state).unwrap_or_default();
            let draft_row = |v: &mut pb::CandidateView| {
                v.agents_md_label = if r.has_agents_md {
                    text::AGENTS_MD_PRESENT
                } else {
                    text::AGENTS_MD_MISSING
                }
                .into();
                v.can_propose_draft = !r.has_agents_md;
                v.propose_draft = !r.has_agents_md && d.propose_draft;
            };
            match state {
                pb::ProjectPathState::NotVersionControlled => {
                    v.notice_title = text::ADD_NOT_VERSIONED_TITLE.into();
                    v.notice_body = text::ADD_NOT_VERSIONED_BODY.into();
                    v.notice_tone = pb::Tone::Warning.into();
                    v.init_label = Some(text::ADD_INIT_LABEL.into());
                    v.initializing = d.busy == Some(Busy::Initializing);
                    draft_row(&mut v);
                }
                pb::ProjectPathState::Addable => {
                    // 选的是仓库的子目录：一行「将添加整个仓库」加根目录
                    if r.root_path != c.path {
                        v.notice_title = text::ADD_SUBDIR_TITLE.into();
                        v.notice_body = text::display_path(&r.root_path, home);
                        v.notice_tone = pb::Tone::Neutral.into();
                    }
                    draft_row(&mut v);
                    v.can_confirm = d.busy.is_none();
                    v.confirm_label = text::ADD_CONFIRM.into();
                }
                pb::ProjectPathState::Known => {
                    v.can_confirm = d.busy.is_none();
                    v.confirm_label = text::ADD_OPEN_KNOWN.into();
                }
                pb::ProjectPathState::Unspecified => {}
            }
        }
    }
    if v.confirm_label.is_empty() {
        v.confirm_label = text::ADD_CONFIRM.into();
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::add_project::Candidate;

    #[test]
    fn display_path_table() {
        let home = Some("/Users/me");
        for (path, want) in [
            ("/Users/me", "~"),
            ("/Users/me/code/svc", "~/code/svc"),
            ("/opt/work", "/opt/work"),
            ("/Users/meg/x", "/Users/meg/x"),
        ] {
            assert_eq!(text::display_path(path, home), want, "{path}");
        }
        assert_eq!(
            text::display_path("/Users/me/a", Some("/Users/me/")),
            "~/a",
            "家目录带尾斜杠"
        );
        assert_eq!(
            text::display_path("/Users/me/a", None),
            "/Users/me/a",
            "没有家目录原样"
        );
    }

    fn done(state: pb::ProjectPathState, root: &str, agents: bool) -> Check {
        Check::Done(pb::InspectProjectPathResult {
            state: state.into(),
            root_path: root.into(),
            name: "x".into(),
            project_id: (state == pb::ProjectPathState::Known).then(|| "p".into()),
            has_agents_md: agents,
        })
    }

    fn view(path: &str, check: Check) -> pb::CandidateView {
        let d = AddProjectDraft::default();
        candidate(
            &Candidate {
                path: path.into(),
                token: 1,
                check,
            },
            &d,
            Some("/h"),
        )
    }

    /// 方案 15.2 的表，每行一例，另加子目录、已有 AGENTS.md 两例；期望值手写。
    #[test]
    fn derive_candidate_table() {
        use pb::ProjectPathState as S;
        // (情形, 提示框标题, 有初始化按钮, AGENTS.md 一行, 显示勾选框, 主按钮可点, 主按钮文字)
        let rows = [
            (
                "检查中",
                view("/h/a", Check::Checking),
                "",
                false,
                "",
                false,
                false,
                "添加并开始对话",
            ),
            (
                "没有版本控制",
                view("/h/a", done(S::NotVersionControlled, "/h/a", false)),
                "这个文件夹还没有版本控制",
                true,
                "未找到",
                true,
                false,
                "添加并开始对话",
            ),
            (
                "能加",
                view("/h/a", done(S::Addable, "/h/a", false)),
                "",
                false,
                "未找到",
                true,
                true,
                "添加并开始对话",
            ),
            (
                "能加、选的是子目录",
                view("/h/a/src", done(S::Addable, "/h/a", false)),
                "将添加整个仓库",
                false,
                "未找到",
                true,
                true,
                "添加并开始对话",
            ),
            (
                "能加、已有 AGENTS.md",
                view("/h/a", done(S::Addable, "/h/a", true)),
                "",
                false,
                "已有",
                false,
                true,
                "添加并开始对话",
            ),
            (
                "已知",
                view("/h/a", done(S::Known, "/h/a", false)),
                "",
                false,
                "",
                false,
                true,
                "打开已有项目",
            ),
            (
                "失败",
                view("/h/a", Check::Failed("找不到这个文件夹".into())),
                "这个文件夹不能添加",
                false,
                "",
                false,
                false,
                "添加并开始对话",
            ),
        ];
        for (case, v, title, init, agents, propose, confirm, label) in rows {
            assert_eq!(v.notice_title, title, "{case}");
            assert_eq!(v.init_label.is_some(), init, "{case}");
            assert_eq!(v.agents_md_label, agents, "{case}");
            assert_eq!(v.can_propose_draft, propose, "{case}");
            assert_eq!(v.can_confirm, confirm, "{case}");
            assert_eq!(v.confirm_label, label, "{case}");
        }
        let sub = view("/h/a/src", done(S::Addable, "/h/a", false));
        assert_eq!(
            (sub.path_label.as_str(), sub.notice_body.as_str()),
            ("~/a", "~/a"),
            "子目录：显示仓库根"
        );
        assert!(view("/h/a", Check::Checking).checking);
    }
}
