//! 添加项目草稿（T09 方案 15.2，RD1、RD4）：选好的文件夹（候选）与它的检查结果、这一次的父目录、「生成 AGENTS.md
//! 草稿」勾没勾、正在做什么、上一次被拒的原因。**绝对路径只在这里**：网页发的意图都不带路径，只看得到派生出的
//! 显示文字（家目录写成 `~`）。路径只从系统选择框、拖入、后台的 `CloneFinished` 进来。
//!
//! 检查（`InspectProjectPath`）是附带查询，结果是某一刻盘上的样子：每换一次候选或重查一次 `token` 加一，
//! 迟到的结果 token 对不上就丢（RD4）。能不能加以 `AddProject` 受理时后台再判一次为准（RD2）：被拒就记下原因并重查。
//!
//! 这里只有状态与决定，不发命令、不发查询；会话层按返回值去做（`session::add_project`）。

use iwork_proto::pb;

/// 一次要发的检查。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspect {
    pub path: String,
    pub token: u64,
}

/// 候选目录的检查结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    Checking,
    Done(pb::InspectProjectPathResult),
    /// 后台给的原因（路径不存在、读不了等）。
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub path: String,
    pub token: u64,
    pub check: Check,
}

/// 正在做的事（主按钮转圈、不可点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    Initializing,
    Adding,
    Creating,
    Cloning,
}

/// 「确认」该做什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// 发 AddProject。
    Add { path: String, propose_draft: bool },
    /// 已经是项目：不发命令，直接打开它。
    Open { project_id: String },
}

/// 当前状态下不能做这件事（回 `IntentAck.error`）。
pub const NOT_READY: &str = "这个文件夹现在还不能这样做";

#[derive(Debug, Clone, PartialEq)]
pub struct AddProjectDraft {
    pub candidate: Option<Candidate>,
    /// 这一次新建、克隆要放到的父目录；None = 用设置里的项目位置。
    pub parent_dir: Option<String>,
    /// 「让 iWork 生成 AGENTS.md 草稿」勾着没。默认勾着（画布 07）。
    pub propose_draft: bool,
    pub busy: Option<Busy>,
    /// 上一次被拒的原因；下一次操作清掉。
    pub error: Option<String>,
    next_token: u64,
}

impl Default for AddProjectDraft {
    fn default() -> Self {
        AddProjectDraft {
            candidate: None,
            parent_dir: None,
            propose_draft: true,
            busy: None,
            error: None,
            next_token: 0,
        }
    }
}

impl AddProjectDraft {
    /// 换一个候选（选择框、拖入、克隆完）：清掉上一次的错误，发检查。
    pub fn set_candidate(&mut self, path: String) -> Inspect {
        self.error = None;
        self.next_token += 1;
        self.candidate = Some(Candidate {
            path: path.clone(),
            token: self.next_token,
            check: Check::Checking,
        });
        Inspect {
            path,
            token: self.next_token,
        }
    }

    /// 重查当前候选（初始化受理后、添加被拒后、断线重连后检查还没完成）。没有候选时什么都不做。
    pub fn reinspect(&mut self) -> Option<Inspect> {
        let path = self.candidate.as_ref()?.path.clone();
        Some(self.set_candidate_keep_error(path))
    }

    fn set_candidate_keep_error(&mut self, path: String) -> Inspect {
        let error = self.error.take();
        let r = self.set_candidate(path);
        self.error = error;
        r
    }

    /// 重连后要重发的检查：有候选、结果还没回来。
    pub fn pending_inspect(&self) -> Option<Inspect> {
        let c = self.candidate.as_ref()?;
        (c.check == Check::Checking).then(|| Inspect {
            path: c.path.clone(),
            token: c.token,
        })
    }

    /// 检查结果回来了：token 对上才记，对不上（候选换过、重查过）丢掉。返回记没记。
    pub fn inspected(
        &mut self,
        token: u64,
        result: Result<pb::InspectProjectPathResult, String>,
    ) -> bool {
        match self.candidate.as_mut() {
            Some(c) if c.token == token => {
                c.check = match result {
                    Ok(r) => Check::Done(r),
                    Err(why) => Check::Failed(why),
                };
                true
            }
            _ => false,
        }
    }

    fn state(&self) -> Option<(pb::ProjectPathState, &pb::InspectProjectPathResult)> {
        match &self.candidate.as_ref()?.check {
            Check::Done(r) => Some((
                pb::ProjectPathState::try_from(r.state).unwrap_or_default(),
                r,
            )),
            _ => None,
        }
    }

    /// 「初始化为仓库」：候选没有版本控制才受理，返回要初始化的目录。
    pub fn init(&mut self) -> Result<String, &'static str> {
        match self.state() {
            Some((pb::ProjectPathState::NotVersionControlled, r)) if self.busy.is_none() => {
                let path = r.root_path.clone();
                self.busy = Some(Busy::Initializing);
                self.error = None;
                Ok(path)
            }
            _ => Err(NOT_READY),
        }
    }

    /// 「添加并开始对话」或「打开已有项目」。
    pub fn confirm(&mut self) -> Result<Confirm, &'static str> {
        if self.busy.is_some() {
            return Err(NOT_READY);
        }
        match self.state() {
            Some((pb::ProjectPathState::Addable, r)) => {
                let c = Confirm::Add {
                    path: r.root_path.clone(),
                    propose_draft: self.propose_draft && !r.has_agents_md,
                };
                self.busy = Some(Busy::Adding);
                self.error = None;
                Ok(c)
            }
            Some((pb::ProjectPathState::Known, r)) => r
                .project_id
                .clone()
                .map(|project_id| Confirm::Open { project_id })
                .ok_or(NOT_READY),
            _ => Err(NOT_READY),
        }
    }

    /// 新建、克隆开始了（忙着，主按钮不可点）。
    pub fn begin(&mut self, busy: Busy) -> Result<(), &'static str> {
        if self.busy.is_some() {
            return Err(NOT_READY);
        }
        self.busy = Some(busy);
        self.error = None;
        Ok(())
    }

    /// 在做的那件事有了结果。`Err` 是后台给的原因。初始化成功、添加被拒都要重查（盘上可能变了）。
    pub fn finished(&mut self, result: Result<(), String>) -> Option<Inspect> {
        let busy = self.busy.take();
        if let Err(why) = result {
            self.error = Some(why);
            return match busy {
                Some(Busy::Adding) => self.reinspect(),
                _ => None,
            };
        }
        match busy {
            Some(Busy::Initializing) => self.reinspect(),
            _ => None,
        }
    }

    /// 弹窗关了：清掉草稿（克隆在后台照常跑，进度不在这里）。
    pub fn clear(&mut self) {
        *self = AddProjectDraft {
            next_token: self.next_token,
            ..Default::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(state: pb::ProjectPathState, agents: bool) -> pb::InspectProjectPathResult {
        pb::InspectProjectPathResult {
            state: state.into(),
            root_path: "/r".into(),
            name: "r".into(),
            project_id: (state == pb::ProjectPathState::Known).then(|| "p1".into()),
            has_agents_md: agents,
        }
    }

    /// RD4：候选换过以后，前一个候选的检查结果迟到，不记。
    #[test]
    fn stale_inspect_result_is_dropped() {
        let mut d = AddProjectDraft::default();
        let a = d.set_candidate("/a".into());
        let b = d.set_candidate("/b".into());
        assert!(!d.inspected(a.token, Ok(result(pb::ProjectPathState::Addable, false))));
        assert_eq!(d.candidate.as_ref().unwrap().check, Check::Checking);
        assert!(d.inspected(b.token, Ok(result(pb::ProjectPathState::Addable, false))));
        assert_eq!(d.candidate.as_ref().unwrap().path, "/b");
    }

    /// 确认：能加 → AddProject（勾着且没有 AGENTS.md 才生成草稿）；已知 → 打开；没版本控制、检查中、忙着 → 不行。
    #[test]
    fn confirm_table() {
        let mut d = AddProjectDraft::default();
        let i = d.set_candidate("/sub".into());
        assert_eq!(d.confirm(), Err(NOT_READY), "检查中");
        d.inspected(
            i.token,
            Ok(result(pb::ProjectPathState::NotVersionControlled, false)),
        );
        assert_eq!(d.confirm(), Err(NOT_READY), "没版本控制");
        let i = d.set_candidate("/sub".into());
        d.inspected(i.token, Ok(result(pb::ProjectPathState::Addable, true)));
        assert_eq!(
            d.confirm(),
            Ok(Confirm::Add {
                path: "/r".into(),
                propose_draft: false
            }),
            "已有 AGENTS.md：勾着也不生成；发的是仓库根"
        );
        assert_eq!(d.confirm(), Err(NOT_READY), "正在加：不能再点");
        d.finished(Ok(()));
        let i = d.set_candidate("/k".into());
        d.inspected(i.token, Ok(result(pb::ProjectPathState::Known, false)));
        assert_eq!(
            d.confirm(),
            Ok(Confirm::Open {
                project_id: "p1".into()
            })
        );
    }

    /// 初始化受理后重查；添加被拒记下原因并重查；初始化被拒只记原因。
    #[test]
    fn finished_reinspects_when_disk_may_have_changed() {
        let mut d = AddProjectDraft::default();
        let i = d.set_candidate("/x".into());
        d.inspected(
            i.token,
            Ok(result(pb::ProjectPathState::NotVersionControlled, false)),
        );
        assert_eq!(d.init(), Ok("/r".into()));
        let again = d.finished(Ok(())).expect("初始化成功后重查");
        assert!(again.token > i.token);
        d.inspected(
            again.token,
            Ok(result(pb::ProjectPathState::Addable, false)),
        );
        d.confirm().unwrap();
        let again = d
            .finished(Err("目录被挪走了".into()))
            .expect("添加被拒后重查");
        assert_eq!(
            d.error.as_deref(),
            Some("目录被挪走了"),
            "原因留着，直到下一次操作"
        );
        d.inspected(
            again.token,
            Ok(result(pb::ProjectPathState::NotVersionControlled, false)),
        );
        d.init().unwrap();
        assert_eq!(d.finished(Err("没配提交身份".into())), None);
        assert_eq!(d.error.as_deref(), Some("没配提交身份"));
        assert!(d.candidate.is_some(), "候选还在");
    }
}
