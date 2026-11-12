//! 崩溃恢复的提交（T04 方案 2.5；设计 4；需求 12「不静默重放」）。
//!
//! 读 `unsettled()` → `iwork_core::recovery::plan`（纯函数）→ 一次提交（事件 + 末尾一条 `RecoveryCompleted`）。
//! 全部事件在一个事务里，要么都生效要么都没有；恢复中途被杀，下次从头再算。没收尾的东西为空就不提交。
//! 只写「它停了」的事实：不重发 prompt、不重跑卡片命令、不动审批。

use iwork_core::recovery::{RecoveryPlan, plan};
use iwork_domain::common::Actor;
use iwork_domain::event::RecoveryCompleted;
use iwork_domain::id::{CommandId, TurnId};
use iwork_ports::clock::IdGenerator;
use iwork_ports::store::CommitRequest;

use crate::boot::{BootError, BootPorts, Step, read_error};
use crate::dev;

/// 恢复提交的动作主体。
pub const RECOVERY_ACTOR: &str = "system:recovery";
/// 开发构建的暂停点：算好计划、提交之前 / 提交之后。
pub const PAUSE_BEFORE_COMMIT: &str = "recovery-before-commit";
pub const PAUSE_AFTER_COMMIT: &str = "recovery-after-commit";

/// 纯函数：计划 → 要提交的请求。计划为空返回 `None`（不提交、不发 `RecoveryCompleted`）。
pub fn recovery_request(plan: RecoveryPlan, command_id: CommandId) -> Option<CommitRequest> {
    if plan.events.is_empty() {
        return None;
    }
    let mut events = plan.events;
    events.push(
        RecoveryCompleted {
            interrupted_turn_ids: plan.interrupted,
        }
        .into(),
    );
    Some(CommitRequest {
        command_id,
        actor: Actor(RECOVERY_ACTOR.into()),
        events,
        outbox: Vec::new(),
    })
}

/// 启动表第 8 步。返回被标成「已中断」的回合。
pub async fn recover(
    ports: &impl BootPorts,
    ids: &impl IdGenerator,
) -> Result<Vec<TurnId>, BootError> {
    let unsettled = ports
        .unsettled()
        .await
        .map_err(|e| read_error(Step::Recovery, e))?;
    // 转换表拒绝说明查询的过滤错了：程序缺陷，以 20 退出，不兜底
    let plan = plan(&unsettled).map_err(|e| BootError::Fatal {
        step: Step::Recovery,
        message: e.to_string(),
    })?;
    let interrupted = plan.interrupted.clone();
    let Some(req) = recovery_request(plan, CommandId::from_raw(ids.next())) else {
        return Ok(interrupted);
    };
    let events = req.events.len();
    dev::pause_at(PAUSE_BEFORE_COMMIT);
    ports.commit(req).await.map_err(|e| BootError::Fatal {
        step: Step::Recovery,
        message: format!("提交恢复事件失败：{e}"),
    })?;
    dev::pause_at(PAUSE_AFTER_COMMIT);
    tracing::info!(events, interrupted = interrupted.len(), "崩溃恢复已提交");
    Ok(interrupted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwork_domain::event::{DomainEvent, RecoveryCompleted};
    use iwork_domain::id::RawUlid;

    #[test]
    fn empty_plan_commits_nothing() {
        let p = RecoveryPlan {
            events: vec![],
            interrupted: vec![],
        };
        assert!(recovery_request(p, CommandId::from_raw(RawUlid::from_u128(1))).is_none());
    }

    #[test]
    fn recovery_completed_is_last_and_lists_interrupted() {
        let t = TurnId::from_raw(RawUlid::from_u128(5));
        let first: DomainEvent = iwork_domain::event::GlobalSettingsChanged {
            playful_ui: None,
            autostart: None,
            projects_root: None,
        }
        .into();
        let p = RecoveryPlan {
            events: vec![first.clone()],
            interrupted: vec![t],
        };
        let req = recovery_request(p, CommandId::from_raw(RawUlid::from_u128(1))).unwrap();
        assert_eq!(req.actor.0, RECOVERY_ACTOR);
        assert_eq!(req.events.len(), 2);
        assert_eq!(req.events[0], first);
        assert_eq!(
            req.events[1],
            RecoveryCompleted {
                interrupted_turn_ids: vec![t]
            }
            .into()
        );
        assert!(req.outbox.is_empty());
    }
}
