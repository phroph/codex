//! Native, durable coordination of task-specific agent workflows.
//!
//! The model owns decomposition and judgment. This module owns scheduling, dependency
//! handoff, interruption, and receipts; a completed worker is not an accepted conclusion.

pub(crate) mod model;
mod progress;
pub(crate) mod store;

use crate::agent::api::AgentControl;
use crate::agent::api::AgentInput;
use crate::agent::api::AgentTarget;
use crate::agent::api::SendRequest;
use crate::agent::api::SpawnRequest;
use crate::agent::api::WorkflowAgentOutcome;
use crate::agent::types::AgentMessage;
use crate::agent::types::MessageDeliveryMode;
use crate::agent::types::SpawnAgentForkMode;
use crate::agent::types::SpawnAgentOptions;
use crate::config::Config;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::thread_spawn::ThreadCreateRequest;
use crate::tools::handlers::multi_agents_v2::emit_sub_agent_activity;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_protocol::turn_input::TurnStartOptions;
use model::RunStatus;
use model::TaskSpec;
use model::TaskStatus;
use model::WorkflowPlan;
use model::WorkflowState;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use store::WorkflowStore;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowArgs {
    pub action: String,
    pub workflow_id: Option<String>,
    pub plan: Option<WorkflowPlan>,
    #[serde(default)]
    pub tasks: Vec<TaskSpec>,
    #[serde(default)]
    pub remove_task_ids: Vec<String>,
    pub goal: Option<String>,
    pub max_parallel: Option<usize>,
    #[serde(default)]
    pub task_ids: Vec<String>,
    pub expected_revision: Option<u64>,
    pub after_revision: Option<u64>,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub include_outputs: bool,
}

#[derive(Default)]
struct WorkflowManager {
    handles: Mutex<BTreeMap<String, WorkflowHandle>>,
    initialization: Mutex<()>,
    progress: Mutex<Option<(String, Value)>>,
}

#[derive(Clone)]
struct WorkflowHandle {
    commands: mpsc::Sender<Command>,
    snapshot: watch::Receiver<WorkflowState>,
    directory: PathBuf,
    observed_terminal: Arc<AtomicBool>,
    stop_requested: Arc<AtomicBool>,
    wake: Arc<Notify>,
    owner_turn: watch::Sender<String>,
}

#[derive(Clone)]
struct PreparedTask {
    config: Config,
    source: SessionSource,
    options: SpawnAgentOptions,
    path: AgentPath,
    turn: Arc<TurnContext>,
}

struct Command {
    action: String,
    tasks: Vec<TaskSpec>,
    remove_task_ids: Vec<String>,
    goal: Option<String>,
    max_parallel: Option<usize>,
    task_ids: Vec<String>,
    expected_revision: Option<u64>,
    prepared: BTreeMap<String, PreparedTask>,
    reply: oneshot::Sender<Result<(), String>>,
}

/// Each actor serializes one graph. It holds a weak owner session, so closing a
/// chat does not keep that session or its subprocesses alive through a cycle.
struct WorkflowActor {
    owner: Weak<Session>,
    owner_id: ThreadId,
    control: Arc<dyn AgentControl>,
    shutdown: CancellationToken,
    state: WorkflowState,
    store: WorkflowStore,
    prepared: BTreeMap<String, PreparedTask>,
    commands: mpsc::Receiver<Command>,
    changed: watch::Sender<WorkflowState>,
    outcomes: broadcast::Receiver<WorkflowAgentOutcome>,
    observed_terminal: Arc<AtomicBool>,
    stop_requested: Arc<AtomicBool>,
    wake: Arc<Notify>,
    owner_idle_turn: Option<String>,
    stop_handled: bool,
    owner_turn: watch::Receiver<String>,
    continued_revision: Option<u64>,
}

/// Explicit stop also applies when the coordinator is idle but its workflow is
/// still executing. Native interruption calls this before acquiring ActiveTurn.
pub(crate) async fn interrupt_owner(session: &Session) -> bool {
    let Some(manager) = session
        .services
        .thread_extension_data
        .get::<WorkflowManager>()
    else {
        return false;
    };
    let handles = manager.handles.lock().await;
    let mut affected = false;
    for handle in handles.values() {
        let state = handle.snapshot.borrow();
        if state.status == RunStatus::Running
            || state
                .tasks
                .values()
                .any(|task| task.status == TaskStatus::Running)
            || (!handle.observed_terminal.load(Ordering::Acquire)
                && matches!(
                    state.status,
                    RunStatus::Completed | RunStatus::NeedsAttention
                ))
        {
            handle.stop_requested.store(true, Ordering::Release);
            handle.wake.notify_one();
            affected = true;
        }
    }
    affected
}

pub(crate) async fn execute(
    session: Arc<Session>,
    step: Arc<StepContext>,
    call_id: &str,
    args: WorkflowArgs,
    cancellation: CancellationToken,
) -> Result<Value, String> {
    let manager = session
        .services
        .thread_extension_data
        .get_or_init(WorkflowManager::default);
    let codex_home = &step.turn.config.codex_home;
    if args.action == "status" && args.workflow_id.is_none() {
        let mut entries = Vec::new();
        let directory = codex_home
            .join("workflows")
            .join(session.thread_id.to_string());
        if let Ok(mut dirs) = tokio::fs::read_dir(&directory).await {
            while let Some(entry) = dirs.next_entry().await.map_err(|e| e.to_string())? {
                let id = entry.file_name().to_string_lossy().into_owned();
                if let Ok(store) =
                    WorkflowStore::new(codex_home, &session.thread_id.to_string(), &id)
                    && let Some(state) = store.load().await?
                {
                    entries.push(json!({"id": id, "goal": state.plan.goal, "status": state.status, "revision": state.revision}));
                }
            }
        }
        return Ok(json!({"workflows": entries, "directory": directory}));
    }
    let id = args
        .workflow_id
        .clone()
        .or_else(|| args.plan.as_ref().map(|p| p.id.clone()))
        .ok_or("workflow_id is required (start may use plan.id)")?;
    let initialization = manager.initialization.lock().await;
    let mut handles = manager.handles.lock().await;
    if args.action == "start" && handles.contains_key(&id) {
        return Err(format!(
            "Workflow {id} already exists; use status or revise"
        ));
    }
    if !handles.contains_key(&id) {
        // Preparation and disk I/O must not hold the map needed by explicit stop.
        drop(handles);
        // Subscribe before any native spawn, including a very fast worker.
        let outcomes = session
            .services
            .agent_control
            .watch_workflow_outcomes()
            .ok_or("The configured agent backend does not support native workflow events")?;
        let store = WorkflowStore::new(codex_home, &session.thread_id.to_string(), &id)?;
        let existing = store.load().await?;
        let (mut state, event) = if args.action == "start" {
            if existing.is_some() {
                return Err(format!(
                    "Workflow {id} already exists on disk; use status, resume, or revise"
                ));
            }
            let plan = args.plan.clone().ok_or("start requires plan")?;
            if plan.id != id {
                return Err("workflow_id and plan.id must match".into());
            }
            (
                model::new_state(plan, session.thread_id.to_string())?,
                "created",
            )
        } else {
            let mut state = existing.ok_or_else(|| format!("Workflow {id} does not exist"))?;
            model::recover_after_restart(&mut state);
            (state, "loaded")
        };
        let prepared = if args.action == "start" {
            prepare_tasks(&session, &step, call_id, &state, &state.plan.tasks).await?
        } else {
            BTreeMap::new()
        };
        if cancellation.is_cancelled() {
            return Err("Workflow call was interrupted before dispatch".into());
        }
        store.save(&mut state, event).await?;
        let (commands_tx, commands) = mpsc::channel(16);
        let (changed, snapshot) = watch::channel(state.clone());
        let observed_terminal = Arc::new(AtomicBool::new(false));
        let stop_requested = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Notify::new());
        let (owner_turn_tx, owner_turn) = watch::channel(step.turn.sub_id.clone());
        let handle = WorkflowHandle {
            commands: commands_tx,
            snapshot,
            directory: store.directory().to_path_buf(),
            observed_terminal: observed_terminal.clone(),
            stop_requested: stop_requested.clone(),
            wake: wake.clone(),
            owner_turn: owner_turn_tx,
        };
        handles = manager.handles.lock().await;
        handles.insert(id.clone(), handle);
        let actor = WorkflowActor {
            owner: Arc::downgrade(&session),
            owner_id: session.thread_id,
            control: session.services.agent_control.clone(),
            shutdown: session.services.local_agent_runtime.shutdown.clone(),
            state,
            store,
            prepared,
            commands,
            changed,
            outcomes,
            observed_terminal,
            stop_requested,
            wake,
            owner_idle_turn: None,
            stop_handled: false,
            owner_turn,
            continued_revision: None,
        };
        tokio::spawn(actor.run());
    }
    let handle = handles
        .get(&id)
        .cloned()
        .ok_or("workflow initialization failed")?;
    drop(handles);
    drop(initialization);
    match args.action.as_str() {
        "start" | "status" => {}
        "wait" => {
            let mut snapshot = handle.snapshot.clone();
            let after = args
                .after_revision
                .unwrap_or_else(|| snapshot.borrow().revision);
            let wait_config = &step.turn.config.multi_agent_v2;
            let requested = args
                .timeout_ms
                .unwrap_or(wait_config.default_wait_timeout_ms as u64);
            if requested > wait_config.max_wait_timeout_ms as u64 {
                return Err(format!(
                    "timeout_ms must be at most {}",
                    wait_config.max_wait_timeout_ms
                ));
            }
            let timeout =
                Duration::from_millis(requested.max(wait_config.min_wait_timeout_ms as u64));
            let (activity, pending_activity) =
                crate::thread_input::subscribe_input_activity(&session, &step.turn.sub_id).await;
            let wait = async {
                loop {
                    let state = snapshot.borrow().clone();
                    if state.revision > after
                        || (state.status != RunStatus::Running
                            && !state
                                .tasks
                                .values()
                                .any(|task| task.status == TaskStatus::Running))
                    {
                        break;
                    }
                    if snapshot.changed().await.is_err() {
                        break;
                    }
                }
            };
            tokio::select! {
                _ = tokio::time::timeout(timeout, wait) => {},
                _ = crate::thread_input::wait_for_activity(activity, pending_activity) => {},
                _ = cancellation.cancelled() => {},
            }
        }
        "revise" | "retry" | "pause" | "resume" | "cancel" => {
            let state = handle.snapshot.borrow().clone();
            let specs = match args.action.as_str() {
                "revise" => args.tasks.clone(),
                "retry" => {
                    let mut reset = state.clone();
                    model::reset_tasks(&mut reset, &args.task_ids)?;
                    reset
                        .tasks
                        .values()
                        .filter(|t| t.status == TaskStatus::Pending)
                        .map(|t| t.spec.clone())
                        .collect()
                }
                "resume" => state
                    .tasks
                    .values()
                    .filter(|t| t.status == TaskStatus::Pending)
                    .map(|t| t.spec.clone())
                    .collect(),
                _ => Vec::new(),
            };
            let prepared = prepare_tasks(&session, &step, call_id, &state, &specs).await?;
            let (reply, receive) = oneshot::channel();
            handle
                .commands
                .send(Command {
                    action: args.action,
                    tasks: args.tasks,
                    remove_task_ids: args.remove_task_ids,
                    goal: args.goal,
                    max_parallel: args.max_parallel,
                    task_ids: args.task_ids,
                    expected_revision: args.expected_revision,
                    prepared,
                    reply,
                })
                .await
                .map_err(|_| "workflow scheduler has stopped; reload the chat before resuming")?;
            receive
                .await
                .map_err(|_| "workflow scheduler stopped during the command")??;
        }
        _ => {
            return Err(
                "action must be start, status, wait, revise, retry, pause, resume, or cancel"
                    .into(),
            );
        }
    }
    let state = handle.snapshot.borrow().clone();
    handle.observed_terminal.store(
        matches!(
            state.status,
            RunStatus::Completed | RunStatus::NeedsAttention
        ),
        Ordering::Release,
    );
    handle.owner_turn.send_replace(step.turn.sub_id.clone());
    progress::publish(&session, &step.turn.sub_id).await;
    Ok(view(&state, &handle.directory, args.include_outputs))
}

fn native_task_name(workflow_id: &str, task_id: &str, attempt: u32) -> String {
    // Workflow IDs allow uppercase letters and hyphens; native agent names do
    // not. Escape underscores too, so distinct IDs never collapse to one path.
    // Encoded components cannot contain `__`, which separates their boundaries.
    let encode = |id: &str| {
        let mut encoded = String::new();
        for byte in id.bytes() {
            if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
                encoded.push(char::from(byte));
            } else {
                encoded.push_str(&format!("_{byte:02x}"));
            }
        }
        encoded
    };
    format!("wf_{}__{}__{attempt}", encode(workflow_id), encode(task_id))
}

async fn prepare_tasks(
    session: &Session,
    step: &StepContext,
    call_id: &str,
    state: &WorkflowState,
    specs: &[TaskSpec],
) -> Result<BTreeMap<String, PreparedTask>, String> {
    let mut prepared = BTreeMap::new();
    for task in specs {
        let attempt = state.tasks.get(&task.id).map_or(1, |t| t.attempt + 1);
        let task_name = native_task_name(&state.plan.id, &task.id, attempt);
        let (settings, path) = crate::thread_spawn::prepare(
            session,
            step,
            call_id,
            ThreadCreateRequest {
                task_name,
                fork_mode: task
                    .inherit_context
                    .then_some(SpawnAgentForkMode::FullHistory),
                agent_type: None,
                model: task.model.clone(),
                reasoning_effort: task.reasoning_effort.clone(),
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        prepared.insert(
            task.id.clone(),
            PreparedTask {
                config: settings.config,
                source: settings.source,
                options: settings.options,
                path,
                turn: step.turn.clone(),
            },
        );
    }
    Ok(prepared)
}

fn view(state: &WorkflowState, directory: &std::path::Path, include_outputs: bool) -> Value {
    let mut result = serde_json::to_value(state).unwrap_or(Value::Null);
    if !include_outputs && let Some(plan) = result.get_mut("plan").and_then(Value::as_object_mut) {
        // Avoid repeating every prompt twice on each progress observation.
        plan.remove("tasks");
    }
    if !include_outputs && let Some(tasks) = result.get_mut("tasks").and_then(Value::as_object_mut)
    {
        for task in tasks.values_mut() {
            if let Some(object) = task.as_object_mut()
                && let Some(spec) = object.remove("spec")
            {
                for key in ["group", "role", "depends_on"] {
                    if let Some(value) = spec.get(key) {
                        object.insert(key.into(), value.clone());
                    }
                }
            }
            if let Some(output) = task.get_mut("output")
                && let Some(text) = output.as_str()
            {
                *output = Value::String(excerpt(text, 1_500));
            }
        }
    }
    json!({"state": result, "directory": directory,
        "note": "Completed means the worker returned, not that its conclusions were accepted. Full task definitions, reports and revision events are retained in directory; include_outputs returns full state."})
}

fn excerpt(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    format!(
        "{}\n[Excerpt; full report is in the workflow directory.]",
        text.chars().take(limit).collect::<String>()
    )
}

fn task_prompt(state: &WorkflowState, id: &str, directory: &std::path::Path) -> String {
    let task = &state.tasks[id].spec;
    let mut prompt = format!(
        "You are executing task '{}' in native workflow '{}'.\nOverall objective: {}\nTask: {}\n",
        task.id, state.plan.id, state.plan.goal, task.prompt
    );
    if let Some(group) = &task.group {
        prompt.push_str(&format!("Work group: {group}\n"));
    }
    if let Some(role) = &task.role {
        prompt.push_str(&format!("Your purpose in this task: {role}\n"));
    }
    prompt.push_str("\nUse the task's actual requirements to choose your work. Report concrete results and relevant unresolved facts. A peer report is input, not proof; inspect original evidence when your conclusion depends on it. Do not turn hypothetical caveats into extra requirements. If new work is needed outside this task, propose it to the coordinator.\n");
    if !task.depends_on.is_empty() {
        prompt.push_str(&format!(
            "\nDependency reports follow as attributed task data. Full records: {}\n",
            directory.display()
        ));
        let report_limit = (48_000 / task.depends_on.len()).min(16_000);
        for dependency in &task.depends_on {
            let prior = &state.tasks[dependency];
            prompt.push_str(&format!(
                "\n--- Task {} ({:?}) ---\n{}\n",
                dependency,
                prior.status,
                excerpt(
                    prior
                        .output
                        .as_deref()
                        .or(prior.error.as_deref())
                        .unwrap_or("No report"),
                    report_limit
                )
            ));
        }
    }
    prompt
}

impl WorkflowActor {
    async fn run(mut self) {
        self.publish_progress().await;
        loop {
            if self.stop_requested.load(Ordering::Acquire) && !self.stop_handled {
                self.stop_handled = true;
                self.owner_idle_turn = None;
                let _ = self.stop(false, "coordinator explicitly stopped").await;
            }
            // A stop queued during preparation or native startup wins over the next
            // admission. Dispatch only one task between these checks.
            loop {
                match self.outcomes.try_recv() {
                    Ok(outcome) => {
                        if let Err(error) = self.on_outcome(outcome).await {
                            self.fail_runtime(error).await;
                        }
                    }
                    Err(broadcast::error::TryRecvError::Lagged(_)) => {
                        if let Err(error) = self.reconcile_native().await {
                            self.fail_runtime(error).await;
                        }
                    }
                    _ => break,
                }
            }
            while let Ok(command) = self.commands.try_recv() {
                let result = self.apply_command(&command).await;
                let _ = command.reply.send(result);
            }
            if self.shutdown.is_cancelled() || self.owner.upgrade().is_none() {
                let _ = self.stop(false, "owner session or agent tree closed").await;
                break;
            }
            match self.dispatch_ready().await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => self.fail_runtime(error).await,
            }
            tokio::select! {
                _ = self.wake.notified() => {},
                _ = self.shutdown.cancelled() => {
                    let _ = self.stop(false, "agent tree shutdown").await;
                    break;
                },
                command = self.commands.recv() => {
                    let Some(command) = command else {
                        let _ = self.stop(false, "owner session closed").await;
                        break;
                    };
                    let result = self.apply_command(&command).await;
                    let _ = command.reply.send(result);
                },
                outcome = self.outcomes.recv() => {
                    match outcome {
                        Ok(outcome) => {
                            if let Err(error) = self.on_outcome(outcome).await {
                                self.fail_runtime(error).await;
                            }
                        },
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Do not infer completion from a missed notification. Reconcile
                            // the native statuses before admitting any further work.
                            if let Err(error) = self.reconcile_native().await {
                                let _ = self.stop(false, &error).await;
                            }
                        },
                        Err(broadcast::error::RecvError::Closed) => {
                            let _ = self.stop(false, "native agent event stream closed").await;
                            break;
                        },
                    }
                },
            }
        }
    }

    async fn commit(&mut self, mut next: WorkflowState, event: &str) -> Result<(), String> {
        self.store.save(&mut next, event).await?;
        self.state = next;
        self.changed.send_replace(self.state.clone());
        self.publish_progress().await;
        Ok(())
    }

    async fn publish_progress(&self) {
        if let Some(owner) = self.owner.upgrade() {
            let turn_id = self.owner_turn.borrow().clone();
            progress::publish(&owner, &turn_id).await;
        }
    }

    async fn fail_runtime(&mut self, error: String) {
        let _ = self.stop(false, &error).await;
        self.state.status = RunStatus::NeedsAttention;
        self.state.last_error = Some(error);
        self.changed.send_replace(self.state.clone());
        self.publish_progress().await;
        self.notify_owner().await;
        self.maybe_continue_owner().await;
    }

    async fn dispatch_ready(&mut self) -> Result<bool, String> {
        if self.state.status != RunStatus::Running {
            return Ok(false);
        }
        loop {
            let running = self
                .state
                .tasks
                .values()
                .filter(|t| t.status == TaskStatus::Running)
                .count();
            if running >= self.state.plan.max_parallel {
                return Ok(false);
            }
            let Some(id) = model::ready_tasks(&self.state).first().cloned() else {
                break;
            };
            let Some(prepared) = self.prepared.get(&id).cloned() else {
                return Err(format!(
                    "Task {id} needs current dispatch settings; use resume"
                ));
            };
            // This is advisory; native spawn still owns the actual shared-tree reservation.
            if let Err(error) = self
                .control
                .check_turn_admission(MultiAgentVersion::V2, &prepared.source)
            {
                if matches!(error.details(), CodexErrorDetails::AgentLimitReached { .. }) {
                    return Ok(false);
                }
                return Err(error.to_string());
            }
            let prompt = task_prompt(&self.state, &id, self.store.directory());
            let mut next = self.state.clone();
            let task = next.tasks.get_mut(&id).ok_or("ready task missing")?;
            task.status = TaskStatus::Running;
            task.attempt += 1;
            task.error = None;
            self.commit(next, &format!("dispatching:{id}")).await?;
            let result = self
                .control
                .spawn(SpawnRequest {
                    caller: self.owner_id,
                    config: prepared.config,
                    input: AgentInput::Message {
                        message: AgentMessage::Plaintext(prompt),
                        mode: MessageDeliveryMode::TriggerTurn,
                    },
                    source: prepared.source,
                    options: prepared.options,
                })
                .await;
            let mut next = self.state.clone();
            let task = next.tasks.get_mut(&id).ok_or("dispatched task missing")?;
            match result {
                Ok((agent, config)) => {
                    task.agent_id = Some(agent.thread_id.to_string());
                    // Acceptance is a side effect. Retain its receipt even when the
                    // journal fails, so interruption can still reach the worker.
                    if let Err(error) = self.commit(next.clone(), &format!("started:{id}")).await {
                        self.state = next;
                        let _ = self
                            .stop(
                                false,
                                &format!("Could not persist accepted task {id}: {error}"),
                            )
                            .await;
                        return Err(error);
                    }
                    if let Some(owner) = self.owner.upgrade() {
                        emit_sub_agent_activity(
                            &owner,
                            &prepared.turn,
                            SubAgentActivityItem {
                                model: Some(config.model),
                                reasoning_effort: config.reasoning_effort,
                                id: format!(
                                    "workflow_{}_{}_{}",
                                    self.state.plan.id, id, self.state.revision
                                ),
                                agent_thread_id: agent.thread_id,
                                agent_path: prepared.path,
                                kind: SubAgentActivityKind::Started,
                            },
                        )
                        .await;
                    }
                }
                Err(error)
                    if matches!(error.details(), CodexErrorDetails::AgentLimitReached { .. }) =>
                {
                    task.status = TaskStatus::Pending;
                    task.attempt = task.attempt.saturating_sub(1);
                    self.commit(next, &format!("waiting_for_capacity:{id}"))
                        .await?;
                    return Ok(false);
                }
                Err(error) => {
                    task.status = TaskStatus::Failed;
                    task.error = Some(error.to_string());
                    self.commit(next, &format!("spawn_failed:{id}")).await?;
                }
            }
            return Ok(true);
        }
        self.settle().await?;
        // Blocking a failed branch can make a failure-handling task ready even
        // when no worker remains to emit another event.
        Ok(!model::ready_tasks(&self.state).is_empty())
    }

    async fn settle(&mut self) -> Result<(), String> {
        let mut next = self.state.clone();
        let changed = model::reconcile_blocked(&mut next);
        let finished = next.tasks.values().all(|t| model::is_terminal(t.status));
        if finished && next.status == RunStatus::Running {
            next.status = if next
                .tasks
                .values()
                .all(|t| t.status == TaskStatus::Completed)
            {
                RunStatus::Completed
            } else {
                RunStatus::NeedsAttention
            };
        }
        if changed || next.status != self.state.status {
            self.commit(next, "settled").await?;
            self.notify_owner().await;
            self.maybe_continue_owner().await;
        }
        Ok(())
    }

    async fn on_outcome(&mut self, outcome: WorkflowAgentOutcome) -> Result<(), String> {
        if outcome.thread_id == self.owner_id {
            if matches!(
                outcome.status,
                AgentStatus::Interrupted
                    | AgentStatus::Shutdown
                    | AgentStatus::Errored(_)
                    | AgentStatus::NotFound
            ) {
                self.owner_idle_turn = None;
                return self.stop(false, "coordinator interrupted or closed").await;
            }
            if outcome.idle
                && matches!(outcome.status, AgentStatus::Completed(_))
                && outcome.turn_id == *self.owner_turn.borrow()
            {
                self.owner_idle_turn = Some(outcome.turn_id);
                self.maybe_continue_owner().await;
            }
            return Ok(());
        }
        if outcome.idle {
            return Ok(());
        }
        let thread = outcome.thread_id.to_string();
        let Some(id) = self
            .state
            .tasks
            .iter()
            .find(|(_, task)| {
                task.agent_id.as_deref() == Some(&thread)
                    && matches!(
                        task.status,
                        TaskStatus::Running | TaskStatus::Interrupted | TaskStatus::Cancelled
                    )
            })
            .map(|(id, _)| id.clone())
        else {
            return Ok(());
        };
        let mut next = self.state.clone();
        let task = next.tasks.get_mut(&id).ok_or("finished task missing")?;
        match outcome.status {
            AgentStatus::Completed(output) => {
                task.status = TaskStatus::Completed;
                task.output = output;
                task.error = None;
            }
            AgentStatus::Errored(error) => {
                task.status = TaskStatus::Failed;
                task.error = Some(error);
            }
            AgentStatus::Interrupted | AgentStatus::Shutdown | AgentStatus::NotFound => {
                task.status = if next.status == RunStatus::Cancelled {
                    TaskStatus::Cancelled
                } else {
                    TaskStatus::Interrupted
                };
                task.error = Some(
                    "Native agent stopped before completing; explicit retry is required".into(),
                );
            }
            _ => return Ok(()),
        }
        self.commit(next, &format!("outcome:{id}:{}", outcome.turn_id))
            .await?;
        self.settle().await
    }

    async fn apply_command(&mut self, command: &Command) -> Result<(), String> {
        if let Some(expected) = command.expected_revision
            && expected != self.state.revision
        {
            return Err(format!(
                "Workflow changed: expected revision {expected}, current {}. Read status and revise again.",
                self.state.revision
            ));
        }
        match command.action.as_str() {
            "pause" => return self.stop(false, "paused by coordinator").await,
            "cancel" => return self.stop(true, "cancelled by coordinator").await,
            _ => {}
        }
        let mut next = self.state.clone();
        match command.action.as_str() {
            "revise" => {
                if let Some(goal) = &command.goal {
                    next.plan.goal = goal.clone();
                }
                if let Some(max_parallel) = command.max_parallel {
                    next.plan.max_parallel = max_parallel;
                }
                model::revise_tasks_with_removals(
                    &mut next,
                    command.tasks.clone(),
                    &command.remove_task_ids,
                )?;
            }
            "retry" => {
                let affected = model::reset_tasks(&mut next, &command.task_ids)?;
                self.ensure_attempts_stopped(&affected).await?;
            }
            "resume" => {
                if next.status == RunStatus::Cancelled {
                    return Err("Cancelled workflows cannot resume; create a new workflow".into());
                }
            }
            _ => return Err("unsupported workflow mutation".into()),
        }
        next.status = RunStatus::Running;
        next.last_error = None;
        self.commit(next, &command.action).await?;
        self.prepared.extend(command.prepared.clone());
        self.stop_requested.store(false, Ordering::Release);
        self.stop_handled = false;
        self.owner_idle_turn = None;
        self.continued_revision = None;
        Ok(())
    }

    async fn ensure_attempts_stopped(&self, affected: &[String]) -> Result<(), String> {
        for id in affected {
            if let Some(agent_id) = self
                .state
                .tasks
                .get(id)
                .and_then(|task| task.agent_id.as_deref())
                && self
                    .control
                    .workflow_attempt_is_running(
                        ThreadId::from_string(agent_id).map_err(|e| e.to_string())?,
                    )
                    .await
                    .map_err(|e| e.to_string())?
            {
                return Err(format!(
                    "Prior attempt for {id} is still stopping. Wait for its native terminal result before retrying."
                ));
            }
        }
        Ok(())
    }

    async fn stop(&mut self, cancel: bool, reason: &str) -> Result<(), String> {
        let cancel = cancel || self.state.status == RunStatus::Cancelled;
        if matches!(
            self.state.status,
            RunStatus::Completed | RunStatus::Cancelled
        ) && !self
            .state
            .tasks
            .values()
            .any(|task| task.status == TaskStatus::Running)
        {
            return Ok(());
        }
        let active = self
            .state
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Running)
            .filter_map(|t| {
                t.agent_id
                    .as_deref()
                    .and_then(|id| ThreadId::from_string(id).ok())
            })
            .collect::<Vec<_>>();
        let mut next = self.state.clone();
        next.status = if cancel {
            RunStatus::Cancelled
        } else {
            RunStatus::Paused
        };
        next.last_error = Some(reason.into());
        for task in next.tasks.values_mut() {
            if task.status == TaskStatus::Running {
                // An interrupt request is not a drain receipt. Keep this attempt
                // active until its native terminal event, preventing overlapping retries.
                task.error = Some(reason.into());
            } else if cancel && !model::is_terminal(task.status) {
                task.status = TaskStatus::Cancelled;
            }
        }
        // Stop dispatch even if disk persistence fails; cancellation must still reach workers.
        let saved = self.commit(next.clone(), reason).await;
        if saved.is_err() {
            self.state = next;
            self.changed.send_replace(self.state.clone());
            self.publish_progress().await;
        }
        let mut failures = Vec::new();
        for id in active {
            if let Err(error) = self
                .control
                .interrupt(self.owner_id, AgentTarget::Id(id), MultiAgentVersion::V2)
                .await
            {
                failures.push(format!("{id}: {error}"));
            }
        }
        saved?;
        if !failures.is_empty() {
            return Err(format!(
                "Dispatch stopped; worker interruption failed: {}",
                failures.join("; ")
            ));
        }
        Ok(())
    }

    async fn reconcile_native(&mut self) -> Result<(), String> {
        let Some(prepared) = self.prepared.values().next() else {
            return Ok(());
        };
        let agents = self
            .control
            .list(
                self.owner_id,
                prepared.turn.parent_thread_id,
                &prepared.turn.session_source,
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        let active = self
            .state
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Running)
            .filter_map(|t| {
                t.agent_id
                    .as_deref()
                    .and_then(|id| ThreadId::from_string(id).ok())
            })
            .collect::<Vec<_>>();
        for id in active {
            // An Interrupted status may precede abort cleanup. A held native
            // execution guard is still work, even after ActiveTurn is removed.
            if self
                .control
                .workflow_attempt_is_running(id)
                .await
                .map_err(|e| e.to_string())?
            {
                continue;
            }
            let status = agents
                .iter()
                .find(|a| a.thread_id == id)
                .map(|a| a.status.clone())
                .unwrap_or(AgentStatus::NotFound);
            self.on_outcome(WorkflowAgentOutcome {
                thread_id: id,
                turn_id: "reconciled".into(),
                status,
                idle: false,
            })
            .await?;
        }
        Ok(())
    }

    async fn notify_owner(&self) {
        let Some(_owner) = self.owner.upgrade() else {
            return;
        };
        let Some(prepared) = self.prepared.values().next() else {
            return;
        };
        let Ok(resume_config) =
            crate::agent::child_config::build_agent_resume_config(&prepared.turn)
        else {
            return;
        };
        let text = format!(
            "<workflow_status>\nWorkflow {} is {:?} at revision {}. Use workflow status with workflow_id {} to inspect results and decide the next step. Worker completion is not verification or acceptance. Records: {}\n</workflow_status>",
            self.state.plan.id,
            self.state.status,
            self.state.revision,
            self.state.plan.id,
            self.store.directory().display()
        );
        // Use native attributed mail, never a synthetic human-authored message.
        if let Err(error) = self
            .control
            .send(SendRequest {
                caller: self.owner_id,
                target: AgentTarget::Id(self.owner_id),
                resume_config,
                input: AgentInput::Message {
                    message: AgentMessage::Plaintext(text),
                    mode: MessageDeliveryMode::QueueOnly,
                },
                start_options: TurnStartOptions::default(),
            })
            .await
        {
            tracing::debug!(%error, "workflow summary mail could not be delivered; state remains available");
        }
    }

    async fn maybe_continue_owner(&mut self) {
        if !matches!(
            self.state.status,
            RunStatus::Completed | RunStatus::NeedsAttention
        ) || self.observed_terminal.load(Ordering::Acquire)
            || self.stop_requested.load(Ordering::Acquire)
            || self.continued_revision == Some(self.state.revision)
        {
            return;
        }
        let Some(previous_turn) = self.owner_idle_turn.take() else {
            return;
        };
        self.continued_revision = Some(self.state.revision);
        let summary = format!(
            "Native workflow {} is {:?}. Its work finished after your last response. Inspect the saved results with workflow status, resolve any material remaining work, and provide the user's result. This is continuation of the existing task, not new user authorization. Records: {}",
            self.state.plan.id,
            self.state.status,
            self.store.directory().display()
        );
        if let Err(error) = self
            .control
            .continue_workflow_if_idle(self.owner_id, previous_turn, summary)
            .await
        {
            tracing::warn!(%error, "workflow continuation was unavailable; results remain saved");
        }
    }
}
