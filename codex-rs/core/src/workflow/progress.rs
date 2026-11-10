//! Project scheduler state into the existing client checklist, without a model call.

use super::WorkflowManager;
use super::model;
use super::model::RunStatus;
use super::model::TaskState;
use super::model::TaskStatus;
use super::model::WorkflowState;
use crate::session::session::Session;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;

const MAX_VISIBLE_TASKS: usize = 20;

pub(super) async fn publish(session: &Session, turn_id: &str) {
    let Some(manager) = session
        .services
        .thread_extension_data
        .get::<WorkflowManager>()
    else {
        return;
    };
    // Serialize projection and delivery so concurrent workflow actors cannot
    // replace a newer view with an older one. Each turn has its own checklist.
    let mut previous = manager.progress.lock().await;
    let states = {
        let handles = manager.handles.lock().await;
        handles
            .values()
            .filter(|handle| *handle.owner_turn.borrow() == turn_id)
            .map(|handle| handle.snapshot.borrow().clone())
            .collect::<Vec<_>>()
    };
    if states.is_empty() {
        return;
    }
    let update = project(&states);
    let Ok(value) = serde_json::to_value(&update) else {
        return;
    };
    if previous
        .as_ref()
        .is_some_and(|(turn, last)| turn == turn_id && *last == value)
    {
        return;
    }
    session
        .send_event_raw(Event {
            id: turn_id.to_string(),
            msg: EventMsg::PlanUpdate(update),
        })
        .await;
    *previous = Some((turn_id.to_string(), value));
}

fn project(states: &[WorkflowState]) -> UpdatePlanArgs {
    let mut summaries = Vec::new();
    let mut rows = Vec::new();
    for state in states {
        let returned = state
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Completed)
            .count();
        let running = state
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Running)
            .count();
        let waiting = state
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Pending)
            .count();
        let attention = state.tasks.len() - returned - running - waiting;
        let status = match state.status {
            RunStatus::Running => "running",
            RunStatus::Paused => "paused",
            RunStatus::Completed => "completed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::NeedsAttention => "needs attention",
        };
        let mut summary = format!(
            "Workflow {} — {} — {status}: {returned}/{} reports returned, {running} active, {waiting} pending",
            state.plan.id,
            compact(&state.plan.goal, 120),
            state.tasks.len(),
        );
        if attention > 0 {
            summary.push_str(&format!(", {attention} blocked or stopped"));
        }
        if let Some(error) = &state.last_error {
            summary.push_str(&format!(". {}", compact(error, 120)));
        }
        summaries.push(summary);
        for spec in &state.plan.tasks {
            let task = &state.tasks[&spec.id];
            let mut label = String::new();
            if states.len() > 1 {
                label.push_str(&format!("{} / ", state.plan.id));
            }
            if let Some(group) = &spec.group {
                label.push_str(&format!("[{}] ", compact(group, 40)));
            }
            label.push_str(&format!(
                "{}: {} — {}",
                spec.id,
                compact(spec.role.as_deref().unwrap_or(&spec.prompt), 100),
                task_detail(state, task),
            ));
            let (priority, status) = match task.status {
                TaskStatus::Running => (0, StepStatus::InProgress),
                TaskStatus::Completed => (3, StepStatus::Completed),
                TaskStatus::Pending => (2, StepStatus::Pending),
                // The existing UI has no failed/cancelled state. Keep these
                // unchecked and name their actual outcome rather than claim success.
                _ => (1, StepStatus::Pending),
            };
            rows.push((
                priority,
                PlanItemArg {
                    step: label,
                    status,
                },
            ));
        }
    }
    if rows.len() > MAX_VISIBLE_TASKS {
        // Keep active and actionable work visible when a large graph is folded.
        rows.sort_by_key(|(priority, _)| *priority);
        let remaining = rows.split_off(MAX_VISIBLE_TASKS);
        let status = if remaining
            .iter()
            .all(|(_, row)| matches!(row.status, StepStatus::Completed))
        {
            StepStatus::Completed
        } else if remaining
            .iter()
            .any(|(_, row)| matches!(row.status, StepStatus::InProgress))
        {
            StepStatus::InProgress
        } else {
            StepStatus::Pending
        };
        rows.push((
            4,
            PlanItemArg {
                step: format!(
                    "{} additional tasks; full details are available through workflow status",
                    remaining.len()
                ),
                status,
            },
        ));
    }
    UpdatePlanArgs {
        explanation: Some(summaries.join("\n")),
        plan: rows.into_iter().map(|(_, row)| row).collect(),
    }
}

fn task_detail(state: &WorkflowState, task: &TaskState) -> String {
    match task.status {
        TaskStatus::Completed => "report returned".into(),
        TaskStatus::Running if matches!(state.status, RunStatus::Paused | RunStatus::Cancelled) => {
            "stopping".into()
        }
        TaskStatus::Running if task.agent_id.is_none() => "starting".into(),
        TaskStatus::Running => "running".into(),
        TaskStatus::Failed
        | TaskStatus::Interrupted
        | TaskStatus::Cancelled
        | TaskStatus::Blocked => {
            let status = match task.status {
                TaskStatus::Failed => "failed",
                TaskStatus::Interrupted => "interrupted",
                TaskStatus::Cancelled => "cancelled",
                _ => "blocked",
            };
            match &task.error {
                Some(error) => format!("{status}: {}", compact(error, 100)),
                None => status.into(),
            }
        }
        TaskStatus::Pending if state.status == RunStatus::Paused => "paused".into(),
        TaskStatus::Pending if state.status != RunStatus::Running => "waiting for resume".into(),
        TaskStatus::Pending => {
            let waiting = task
                .spec
                .depends_on
                .iter()
                .filter(|id| {
                    let prior = &state.tasks[*id];
                    if task.spec.run_after_failure {
                        !model::is_terminal(prior.status)
                    } else {
                        prior.status != TaskStatus::Completed
                    }
                })
                .cloned()
                .collect::<Vec<_>>();
            if waiting.is_empty() {
                "queued".into()
            } else {
                format!("waiting for {}", compact(&waiting.join(", "), 100))
            }
        }
    }
}

fn compact(text: &str, limit: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= limit {
        text
    } else {
        format!("{}…", text.chars().take(limit).collect::<String>())
    }
}
