//! 项目身份规则（技术设计 8.2，需求 FR19；ADR-0010）：一仓库一项目，去重、重新定位可靠。
//!
//! 只经端口做 I/O：仓库归属问 [`VcsProvider::detect`]，项目 ID 读写 [`ProjectMarkerStore`]。
//! 这里不知道用的是哪种版本控制工具，也不知道项目标记存在哪个文件里。
//!
//! - **去重**按仓库共享目录的真实路径：同一仓库从子目录、符号链接、不同大小写的路径、链接工作树添加，
//!   都归到已登记的项目。共享目录不存进项目实体，每次对已登记项目的根重新 `detect`。
//! - **项目 ID** 首次添加时写进仓库的项目标记；仓库里已有标记（例如移除后重新添加）就沿用它，历史随之恢复。
//! - **重新定位**只认项目 ID：新位置的标记与项目一致才关联。
//! - 项目 ID 只属于本机这份工作副本，写标记时一并让版本控制忽略它（端口实现负责）。万一用户强制提交了
//!   标记、仓库又被另克隆一份，标记里的 ID 已登记给另一个位置的项目：兜底明确拒绝
//!   （[`ProjectIdentityError::IdConflict`]），不覆盖、不换 ID、也不当成重新定位。

use std::path::{Path, PathBuf};

use iwork_domain::DomainError;
use iwork_domain::id::ProjectId;
use iwork_domain::project::{self, PathStatus, PathTrigger, Project};
use iwork_ports::clock::IdGenerator;
use iwork_ports::project_files::{ProjectFilesError, ProjectMarkerStore};
use iwork_ports::vcs::{Detection, RepoRoot, VcsError, VcsProvider};

/// 添加一个目录时它是什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddTarget {
    /// 不在版本控制下：提示初始化，初始化前不能添加
    NotVersionControlled,
    /// 同一仓库已登记：跳转到这个项目
    Existing(ProjectId),
    /// 还没登记的仓库
    New(NewProject),
}

/// 待登记的仓库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProject {
    /// 仓库根（从子目录添加时已归到根）
    pub root: RepoRoot,
    /// 仓库里已有的项目 ID（之前添加过又移除）；`None` 表示登记时新生成
    pub existing_id: Option<ProjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProjectIdentityError {
    #[error(transparent)]
    Vcs(#[from] VcsError),
    #[error(transparent)]
    Files(#[from] ProjectFilesError),
    #[error(transparent)]
    Domain(#[from] DomainError),
    /// 仓库的项目标记里的 ID 已登记给另一个位置的项目（映射 `PROJECT_ID_CONFLICT`）
    #[error(
        "{root} 的项目标记写着 {id}，这个 ID 已登记给位于 {registered_path} 的项目（路径状态 {registered_status:?}）"
    )]
    IdConflict {
        id: ProjectId,
        root: PathBuf,
        registered_path: PathBuf,
        registered_status: PathStatus,
    },
    /// 重新定位的新位置没有项目标记，无法确认是同一个项目（映射 `PROJECT_ID_MISMATCH`）
    #[error("{root} 下没有项目标记，无法确认是同一个项目")]
    MarkerMissing { root: PathBuf },
}

/// 判断添加 `path` 时它是什么。只读，不写任何东西。
///
/// `known` 是当前已登记的全部项目。
pub async fn resolve_add<V: VcsProvider, M: ProjectMarkerStore>(
    vcs: &V,
    markers: &M,
    known: &[Project],
    path: &Path,
) -> Result<AddTarget, ProjectIdentityError> {
    let (root, shared_dir) = match vcs.detect(path).await? {
        Detection::NotVersionControlled => return Ok(AddTarget::NotVersionControlled),
        Detection::Repository { root, shared_dir } => (root, shared_dir),
    };
    for p in known {
        if verified_shared_dir(vcs, markers, p).await?.as_ref() == Some(&shared_dir) {
            return Ok(AddTarget::Existing(p.id));
        }
    }
    let existing_id = markers.read(&root.0).await?;
    if let Some(id) = existing_id
        && let Some(registered) = known.iter().find(|p| p.id == id)
    {
        return Err(ProjectIdentityError::IdConflict {
            id,
            root: root.0,
            registered_path: registered.root_path.clone(),
            registered_status: registered.path_status,
        });
    }
    Ok(AddTarget::New(NewProject { root, existing_id }))
}

/// 已登记项目的登记位置是否仍是**这个项目**：位置仍是仓库根，并且那里的项目标记写着这个项目的 ID。
/// 是就返回仓库的共享目录，否则 `None`。路径状态检查与添加时的去重都按这一条判断。
///
/// 不算「还在」的情况：路径不在了；目录还在但不再受版本控制，或成了别的仓库的子目录；原路径上
/// 换成了另一个仓库（没有标记或标记是别的 ID）；标记读不出 ID 或是符号链接（无法确认是同一个项目，
/// 读不出的原因由重新定位或添加那个目录时的错误报出来）。
async fn verified_shared_dir<V: VcsProvider, M: ProjectMarkerStore>(
    vcs: &V,
    markers: &M,
    p: &Project,
) -> Result<Option<PathBuf>, ProjectIdentityError> {
    let (root, shared_dir) = match vcs.detect(&p.root_path).await {
        Ok(Detection::Repository { root, shared_dir }) => (root, shared_dir),
        Ok(Detection::NotVersionControlled) | Err(VcsError::PathMissing { .. }) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if root.0 != p.root_path {
        return Ok(None);
    }
    match markers.read(&root.0).await {
        Ok(found) if found == Some(p.id) => Ok(Some(shared_dir)),
        Ok(_)
        | Err(
            ProjectFilesError::MarkerMalformed { .. } | ProjectFilesError::SymlinkRefused { .. },
        ) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 登记前确定项目 ID：仓库里已有标记就沿用，否则生成一个并写进仓库。
pub async fn assign_id<M: ProjectMarkerStore, G: IdGenerator>(
    markers: &M,
    ids: &G,
    new: &NewProject,
) -> Result<ProjectId, ProjectIdentityError> {
    if let Some(id) = new.existing_id {
        return Ok(id);
    }
    let id = ProjectId::from_raw(ids.next());
    markers.write(&new.root.0, id).await?;
    Ok(id)
}

/// 启动或打开项目时检查路径，返回变化后的路径状态（没变化返回 `None`）。
///
/// 「还在」按 [`verified_shared_dir`] 判断：原本正常、现在不是了 → 找不到路径；原本找不到、现在又是了
/// → 恢复正常。原路径上换成了别的仓库、或失去了版本控制，都算找不到。
pub async fn check_path<V: VcsProvider, M: ProjectMarkerStore>(
    vcs: &V,
    markers: &M,
    p: &Project,
) -> Result<Option<PathStatus>, ProjectIdentityError> {
    let present = verified_shared_dir(vcs, markers, p).await?.is_some();
    let trigger = match (p.path_status, present) {
        (PathStatus::Ok, false) => PathTrigger::PathLost,
        (PathStatus::Missing, true) => PathTrigger::PathFound,
        _ => return Ok(None),
    };
    Ok(Some(
        project::next(p.path_status, trigger).map_err(DomainError::from)?,
    ))
}

/// 把项目重新关联到用户选的新位置。新位置须在版本控制下、有项目标记且 ID 与项目一致，
/// 否则返回错误且项目不变。成功时项目的根改为新位置的仓库根、路径状态恢复正常。
pub async fn relocate<V: VcsProvider, M: ProjectMarkerStore>(
    vcs: &V,
    markers: &M,
    p: &mut Project,
    new_path: &Path,
) -> Result<(), ProjectIdentityError> {
    let root = match vcs.detect(new_path).await? {
        Detection::Repository { root, .. } => root,
        Detection::NotVersionControlled => {
            return Err(VcsError::NotVersionControlled {
                path: new_path.to_owned(),
            }
            .into());
        }
    };
    let Some(found) = markers.read(&root.0).await? else {
        return Err(ProjectIdentityError::MarkerMissing { root: root.0 });
    };
    p.relocate(root.0, found)?;
    Ok(())
}
