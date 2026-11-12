//! Durable workflow state and dependency rules, independent of agent execution.

use codex_protocol::openai_models::ReasoningEffort;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub(crate) const SCHEMA_VERSION: u32 = 1;
const MAX_TASKS: usize = 128;
const MAX_PARALLEL: usize = 32;
const MAX_TEXT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowPlan {
    pub id: String,
    pub goal: String,
    pub tasks: Vec<TaskSpec>,
    #[serde(default = "default_max_parallel")]
    pub max_parallel: usize,
}

fn default_max_parallel() -> usize {
    3
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskSpec {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub run_after_failure: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub inherit_context: bool,
    #[serde(default)]
    pub kind: TaskKind,
    #[serde(default)]
    pub deliverable: Option<String>,
    #[serde(default)]
    pub inputs: Vec<TaskInput>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskKind {
    #[default]
    Work,
    Design,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskInput {
    pub task_id: String,
    pub use_for: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DesignState {
    pub round: u32,
    pub strategy: String,
    pub completion_criteria: Vec<String>,
    pub deferred_decisions: Vec<String>,
    pub assessment: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunStatus {
    Running,
    AwaitingDecision,
    Paused,
    Completed,
    Cancelled,
    NeedsAttention,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Interrupted,
    Cancelled,
    Blocked,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskState {
    pub spec: TaskSpec,
    pub status: TaskStatus,
    pub agent_id: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub attempt: u32,
}

impl TaskState {
    fn pending(spec: TaskSpec) -> Self {
        Self {
            spec,
            status: TaskStatus::Pending,
            agent_id: None,
            output: None,
            error: None,
            attempt: 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowState {
    pub schema_version: u32,
    pub plan: WorkflowPlan,
    pub owner_thread_id: String,
    pub revision: u64,
    pub status: RunStatus,
    #[serde(default)]
    pub last_error: Option<String>,
    pub tasks: BTreeMap<String, TaskState>,
    // Missing in pre-design journals. Loading those workflows preserves their lifecycle.
    #[serde(default)]
    pub design: Option<DesignState>,
}

/// Keep identifiers portable and safe as individual filesystem components.
pub(crate) fn validate_id(id: &str, field: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Err(format!(
            "{field} must contain 1–64 ASCII letters, digits, underscores, or hyphens"
        ));
    }
    let upper = id.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit());
    if reserved {
        return Err(format!("{field} is a reserved filesystem name"));
    }
    Ok(())
}

pub(crate) fn validate_plan(plan: &WorkflowPlan) -> Result<(), String> {
    validate_id(&plan.id, "workflow id")?;
    validate_text(&plan.goal, "workflow goal")?;
    if plan.tasks.is_empty() || plan.tasks.len() > MAX_TASKS {
        return Err(format!(
            "a workflow must contain between 1 and {MAX_TASKS} tasks"
        ));
    }
    if plan.max_parallel == 0 || plan.max_parallel > MAX_PARALLEL {
        return Err(format!("max_parallel must be between 1 and {MAX_PARALLEL}"));
    }
    let mut ids = BTreeSet::new();
    let mut folded_ids = BTreeSet::new();
    for task in &plan.tasks {
        validate_id(&task.id, "task id")?;
        if !ids.insert(task.id.as_str()) || !folded_ids.insert(task.id.to_ascii_lowercase()) {
            return Err(format!("duplicate task id (case-insensitive): {}", task.id));
        }
        validate_text(&task.prompt, &format!("prompt for {}", task.id))?;
        for (name, value) in [
            ("group", task.group.as_ref()),
            ("role", task.role.as_ref()),
            ("model", task.model.as_ref()),
        ] {
            if let Some(value) = value
                && (value.trim().is_empty() || value.len() > 1024)
            {
                return Err(format!("{name} for {} must contain 1–1024 bytes", task.id));
            }
        }
        if let Some(deliverable) = &task.deliverable {
            validate_text(deliverable, &format!("deliverable for {}", task.id))?;
        }
        let mut inputs = BTreeSet::new();
        for input in &task.inputs {
            validate_text(&input.use_for, "input use")?;
            if !task.depends_on.contains(&input.task_id) || !inputs.insert(&input.task_id) {
                return Err(format!(
                    "task {} has an input without a unique dependency: {}",
                    task.id, input.task_id
                ));
            }
        }
    }
    for task in &plan.tasks {
        let mut dependencies = BTreeSet::new();
        for dependency in &task.depends_on {
            if !ids.contains(dependency.as_str()) {
                return Err(format!(
                    "task {} has unknown dependency {dependency}",
                    task.id
                ));
            }
            if dependency == &task.id {
                return Err(format!("task {} depends on itself", task.id));
            }
            if !dependencies.insert(dependency) {
                return Err(format!("task {} repeats dependency {dependency}", task.id));
            }
        }
    }
    // The graph is small and bounded; a stable topological pass also gives useful errors.
    let mut resolved = BTreeSet::new();
    loop {
        let before = resolved.len();
        for task in &plan.tasks {
            if task
                .depends_on
                .iter()
                .all(|id| resolved.contains(id.as_str()))
            {
                resolved.insert(task.id.as_str());
            }
        }
        if resolved.len() == plan.tasks.len() {
            return Ok(());
        }
        if before == resolved.len() {
            let blocked: Vec<_> = plan
                .tasks
                .iter()
                .filter(|task| !resolved.contains(task.id.as_str()))
                .map(|task| task.id.as_str())
                .collect();
            return Err(format!(
                "workflow dependencies contain a cycle involving {}",
                blocked.join(", ")
            ));
        }
    }
}

fn validate_text(text: &str, field: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
        return Err(format!("{field} must contain 1–{MAX_TEXT_BYTES} bytes"));
    }
    Ok(())
}

pub(crate) fn new_state(
    plan: WorkflowPlan,
    owner_thread_id: String,
) -> Result<WorkflowState, String> {
    validate_plan(&plan)?;
    validate_id(&owner_thread_id, "owner thread id")?;
    let tasks = plan
        .tasks
        .iter()
        .cloned()
        .map(|spec| (spec.id.clone(), TaskState::pending(spec)))
        .collect();
    Ok(WorkflowState {
        schema_version: SCHEMA_VERSION,
        plan,
        owner_thread_id,
        revision: 0,
        status: RunStatus::Running,
        last_error: None,
        tasks,
        design: None,
    })
}

pub(crate) fn is_terminal(status: TaskStatus) -> bool {
    !matches!(status, TaskStatus::Pending | TaskStatus::Running)
}

pub(super) fn dependency_ready(state: &WorkflowState, spec: &TaskSpec, id: &str) -> bool {
    let Some(dependency) = state.tasks.get(id) else {
        return false;
    };
    dependency.status == TaskStatus::Completed
        || (is_terminal(dependency.status)
            && (spec.run_after_failure
                // Explicit advancement has already accepted these prior results as
                // planning inputs. Their failure reports can inform recovery work;
                // failures inside the new frontier still block their consumers.
                || (spec.kind == TaskKind::Work && spec.depends_on.iter().any(|planner_id| {
                    state.tasks.get(planner_id).is_some_and(|planner| {
                        planner.spec.kind == TaskKind::Design
                            && planner.status == TaskStatus::Completed
                            && planner.spec.depends_on.contains(&id.to_string())
                    })
                }))))
}

pub(crate) fn ready_tasks(state: &WorkflowState) -> Vec<String> {
    if state.status != RunStatus::Running {
        return Vec::new();
    }
    let running = state
        .tasks
        .values()
        .filter(|task| task.status == TaskStatus::Running)
        .count();
    let capacity = state.plan.max_parallel.saturating_sub(running);
    state
        .plan
        .tasks
        .iter()
        .filter(|spec| {
            state
                .tasks
                .get(&spec.id)
                .is_some_and(|task| task.status == TaskStatus::Pending)
                && spec
                    .depends_on
                    .iter()
                    .all(|id| dependency_ready(state, spec, id))
        })
        .take(capacity)
        .map(|spec| spec.id.clone())
        .collect()
}

/// Propagate failed dependencies, but leave explicitly designated failure handlers runnable.
/// A later explicit reset/revision can clear Blocked without losing the original failure.
pub(crate) fn reconcile_blocked(state: &mut WorkflowState) -> bool {
    let mut any_changed = false;
    loop {
        let blocked: Vec<_> = state
            .tasks
            .iter()
            .filter_map(|(id, task)| {
                if task.status != TaskStatus::Pending || task.spec.run_after_failure {
                    return None;
                }
                let failed: Vec<_> = task
                    .spec
                    .depends_on
                    .iter()
                    .filter(|dependency| {
                        state.tasks.get(*dependency).is_some_and(|prior| {
                            is_terminal(prior.status)
                                && !dependency_ready(state, &task.spec, dependency)
                        })
                    })
                    .cloned()
                    .collect();
                (!failed.is_empty()).then(|| (id.clone(), failed))
            })
            .collect();
        if blocked.is_empty() {
            break;
        }
        for (id, failed) in blocked {
            if let Some(task) = state.tasks.get_mut(&id) {
                task.status = TaskStatus::Blocked;
                task.error = Some(format!(
                    "dependency did not complete successfully: {}",
                    failed.join(", ")
                ));
                any_changed = true;
            }
        }
    }
    any_changed
}

/// Add tasks or revise work that has not started. Validation happens before committing changes.
#[cfg(test)]
pub(crate) fn revise_tasks(
    state: &mut WorkflowState,
    updates: Vec<TaskSpec>,
) -> Result<(), String> {
    revise_tasks_with_removals(state, updates, &[])
}

/// Rewrite dependencies and remove obsolete branches as one validated graph change.
pub(crate) fn revise_tasks_with_removals(
    state: &mut WorkflowState,
    updates: Vec<TaskSpec>,
    removals: &[String],
) -> Result<(), String> {
    if state.status == RunStatus::Cancelled {
        return Err("cancelled workflows cannot be revised".to_string());
    }
    let mut candidate = state.clone();
    let mut removal_ids = BTreeSet::new();
    for id in removals {
        if !removal_ids.insert(id.as_str()) {
            return Err(format!("task {id} was supplied for removal more than once"));
        }
        let existing = candidate
            .tasks
            .get(id)
            .ok_or_else(|| format!("unknown task {id}"))?;
        if !matches!(existing.status, TaskStatus::Pending | TaskStatus::Blocked)
            || existing.attempt != 0
        {
            return Err(format!(
                "task {id} cannot be removed; only unstarted pending or blocked tasks can be removed"
            ));
        }
        candidate.tasks.remove(id);
    }
    candidate
        .plan
        .tasks
        .retain(|task| !removal_ids.contains(task.id.as_str()));
    let mut update_ids = BTreeSet::new();
    for spec in updates {
        if removal_ids.contains(spec.id.as_str()) {
            return Err(format!(
                "task {} cannot be updated and removed together",
                spec.id
            ));
        }
        if !update_ids.insert(spec.id.clone()) {
            return Err(format!("task {} was supplied more than once", spec.id));
        }
        if let Some(existing) = candidate.tasks.get(&spec.id) {
            if !matches!(existing.status, TaskStatus::Pending | TaskStatus::Blocked) {
                return Err(format!(
                    "task {} has already started; reset it explicitly before revising",
                    spec.id
                ));
            }
            let index = candidate
                .plan
                .tasks
                .iter()
                .position(|task| task.id == spec.id)
                .ok_or_else(|| format!("task {} is missing from the workflow plan", spec.id))?;
            candidate.plan.tasks[index] = spec.clone();
            let attempt = existing.attempt;
            let mut pending = TaskState::pending(spec.clone());
            pending.attempt = attempt;
            candidate.tasks.insert(spec.id.clone(), pending);
        } else {
            candidate.plan.tasks.push(spec.clone());
            candidate
                .tasks
                .insert(spec.id.clone(), TaskState::pending(spec));
        }
    }
    validate_plan(&candidate.plan)?;
    // Recompute downstream blocks when an unstarted dependency was revised.
    for task in candidate.tasks.values_mut() {
        if task.status == TaskStatus::Blocked {
            task.status = TaskStatus::Pending;
            task.error = None;
        }
    }
    reconcile_blocked(&mut candidate);
    *state = candidate;
    Ok(())
}

/// Explicitly retry the requested work and invalidate every result that depended on it.
pub(crate) fn reset_tasks(
    state: &mut WorkflowState,
    requested: &[String],
) -> Result<Vec<String>, String> {
    if state.status == RunStatus::Cancelled {
        return Err("cancelled workflows cannot be retried".to_string());
    }
    if requested.is_empty() {
        return Err("at least one task must be specified for retry".to_string());
    }
    let mut affected = BTreeSet::new();
    for id in requested {
        if !state.tasks.contains_key(id) {
            return Err(format!("unknown task {id}"));
        }
        if state.tasks[id].spec.kind == TaskKind::Design
            && state.tasks[id].status == TaskStatus::Completed
        {
            return Err(
                "A completed design cannot be replayed; use advance with the remaining objective"
                    .into(),
            );
        }
        affected.insert(id.clone());
    }
    loop {
        let before = affected.len();
        for (id, task) in &state.tasks {
            if task
                .spec
                .depends_on
                .iter()
                .any(|dependency| affected.contains(dependency))
            {
                affected.insert(id.clone());
            }
        }
        if before == affected.len() {
            break;
        }
    }
    for id in &affected {
        if state.tasks.get(id).is_some_and(|task| {
            task.spec.kind == TaskKind::Design && task.status == TaskStatus::Completed
        }) {
            return Err("Those results were consumed by a later design; use advance to plan the recheck and its follow-on work".into());
        }
        if state
            .tasks
            .get(id)
            .is_some_and(|task| task.status == TaskStatus::Running)
        {
            return Err(format!(
                "task {id} is running; pause or interrupt it before retrying its dependencies"
            ));
        }
    }
    for id in &affected {
        if let Some(task) = state.tasks.get_mut(id) {
            task.status = TaskStatus::Pending;
            task.agent_id = None;
            task.output = None;
            task.error = None;
        }
    }
    reconcile_blocked(state);
    Ok(affected.into_iter().collect())
}

/// A process restart is not authorization to replay an interrupted task's side effects.
pub(crate) fn recover_after_restart(state: &mut WorkflowState) -> bool {
    let mut changed = false;
    for task in state.tasks.values_mut() {
        if task.status == TaskStatus::Running {
            task.status = TaskStatus::Interrupted;
            task.error = Some("workflow execution stopped before a durable result was recorded; inspect the prior attempt before retrying".to_string());
            changed = true;
        }
    }
    if state.status == RunStatus::Running {
        state.status = RunStatus::NeedsAttention;
        state.last_error = Some(
            "workflow execution stopped; inspect interrupted attempts before explicitly resuming"
                .to_string(),
        );
        changed = true;
    } else if changed && state.status != RunStatus::Cancelled {
        state.status = RunStatus::NeedsAttention;
        state.last_error = Some("workflow execution stopped with an unfinished task; inspect the prior attempt before retrying".to_string());
    }
    changed |= reconcile_blocked(state);
    changed
}

pub(crate) fn validate_state(state: &WorkflowState) -> Result<(), String> {
    if state.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported workflow schema version {}",
            state.schema_version
        ));
    }
    validate_plan(&state.plan)?;
    validate_id(&state.owner_thread_id, "owner thread id")?;
    if state.tasks.len() != state.plan.tasks.len() {
        return Err("workflow state task count does not match the plan".to_string());
    }
    for spec in &state.plan.tasks {
        let task = state
            .tasks
            .get(&spec.id)
            .ok_or_else(|| format!("workflow state is missing task {}", spec.id))?;
        if &task.spec != spec {
            return Err(format!("task {} does not match its workflow plan", spec.id));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
