//! Goal-first workflow design through an ordinary native agent with typed output.

use super::model;
use super::model::DesignState;
use super::model::RunStatus;
use super::model::TaskInput;
use super::model::TaskKind;
use super::model::TaskSpec;
use super::model::TaskStatus;
use super::model::WorkflowPlan;
use super::model::WorkflowState;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DesignedTask {
    id: String,
    group: String,
    role: String,
    prompt: String,
    deliverable: String,
    inputs: Vec<TaskInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DesignOutput {
    strategy: String,
    completion_criteria: Vec<String>,
    deferred_decisions: Vec<String>,
    tasks: Vec<DesignedTask>,
}

pub(super) fn output_schema() -> Value {
    let text = json!({"type":"string"});
    let strings = json!({"type":"array","items":text});
    json!({
        "type":"object", "additionalProperties":false,
        "properties":{
            "strategy":text,
            "completion_criteria":strings,
            "deferred_decisions":strings,
            "tasks":{"type":"array","items":{
                "type":"object", "additionalProperties":false,
                "properties":{
                    "id":text,"group":text,"role":text,"prompt":text,"deliverable":text,
                    "inputs":{"type":"array","items":{
                        "type":"object","additionalProperties":false,
                        "properties":{"task_id":text,"use_for":text},
                        "required":["task_id","use_for"]
                    }}
                },
                "required":["id","group","role","prompt","deliverable","inputs"]
            }}
        },
        "required":["strategy","completion_criteria","deferred_decisions","tasks"]
    })
}

const DESIGN_INSTRUCTIONS: &str = r#"Design the executable workflow for the owner's objective. Your final response MUST match the provided JSON schema. Do not execute the project work, write project files, dispatch agents, or create another workflow. You may inspect a small amount of relevant source or evidence to ground the design; use the inherited user request and constraints.

Plan the reasoning and information flow, not merely a division of topics. Start from the outcome the user needs and the evidence or artifacts that determine it. For each task, name its concrete deliverable. In inputs, name each other task's deliverable it needs and explain exactly how that result changes its work. The runtime turns these inputs into enforced dependencies and supplies the actual reports. Independent tasks have empty inputs and may run concurrently.

When several investigations jointly determine a diagnosis, recommendation, implementation, or acceptance decision, design the useful downstream integration/check/implementation task that consumes them. Its deliverable should resolve a concrete cross-branch question against underlying evidence, test a mechanism, or produce an artifact needed for the objective. Do not create a task merely to draft the owner's final answer or repeat the reports: final synthesis belongs to the owner. Do not hide an entire discovery, decision, implementation and verification chain inside each parallel specialist. Conversely, do not add a verifier or critic just for ceremony, require a fixed role sequence, or create dependencies whose outputs are not used. Groups describe meaningful work stages or branches; labels alone do not sequence work.

Choose the smallest useful graph for this objective. A broad audit often needs a shared current-state/evidence baseline, independent analyses, and a later reconciliation of actual findings; a repair may need diagnosis before implementation and a focused behavior check afterward. These are examples, not templates. Make any independent check concrete about the claim or artifact it will inspect. Keep the owner's final judgment with the owner.

If the contents of the first results must determine what to do next, plan only that useful frontier and put the exact decision/questions in deferred_decisions. The runtime will stop at a coordinator checkpoint after this graph returns. The owner can advance with actual reports to design the next frontier. Do not invent future findings or repeat completed work. On a later planning round, use those reports and the owner's assessment to add only necessary remaining tasks. Return no tasks only if existing results now suffice for the owner's final synthesis; explain this in strategy. Initial designs must contain actual work.

Use unique short task IDs and refer only to planned task IDs or existing work task IDs supplied below. Do not reference internal _design_ tasks. Include relevant file paths, scope, user constraints, and practical completion criteria in task prompts; downstream fresh agents must be able to act on their assignment and supplied reports. Your design does not grant new authority or alter the user's constraints. No model or permission overrides are part of this output."#;

fn design_task(round: u32, context: String, dependencies: Vec<String>) -> TaskSpec {
    TaskSpec {
        id: format!("_design_{round}"),
        prompt: format!("{DESIGN_INSTRUCTIONS}\n\nCoordinator context:\n{context}"),
        group: Some("Workflow design".into()),
        role: Some(
            if round == 1 {
                "Design the work and information dependencies"
            } else {
                "Design next work from returned evidence"
            }
            .into(),
        ),
        depends_on: dependencies,
        run_after_failure: true,
        model: None,
        reasoning_effort: None,
        inherit_context: true,
        kind: TaskKind::Design,
        deliverable: Some(
            "Structured workflow design with concrete outputs and their consumers".into(),
        ),
        inputs: Vec::new(),
    }
}

pub(super) fn start(
    id: String,
    goal: String,
    max_parallel: usize,
    proposal: Option<&WorkflowPlan>,
    owner: String,
) -> Result<WorkflowState, String> {
    let context = match proposal {
        Some(proposal) => format!(
            "The caller supplied a candidate graph. Treat it as a proposal, not an instruction to preserve its decomposition. Design against the actual objective.\n{}",
            serde_json::to_string(proposal).map_err(|e| e.to_string())?
        ),
        None => "Use the user's objective and constraints; no graph has been prescribed.".into(),
    };
    let mut state = model::new_state(
        WorkflowPlan {
            id,
            goal,
            max_parallel,
            tasks: vec![design_task(1, context, Vec::new())],
        },
        owner,
    )?;
    state.design = Some(DesignState {
        round: 1,
        ..Default::default()
    });
    Ok(state)
}

pub(super) fn advance(state: &mut WorkflowState, assessment: &str) -> Result<TaskSpec, String> {
    if !matches!(
        state.status,
        RunStatus::AwaitingDecision | RunStatus::NeedsAttention
    ) {
        return Err("advance requires awaiting_decision or needs_attention; finish or stop the current work first".into());
    }
    if state
        .tasks
        .values()
        .any(|task| !model::is_terminal(task.status))
    {
        return Err(
            "advance cannot bypass unfinished work; resolve pending/running tasks first".into(),
        );
    }
    nonempty(assessment, "assessment for advancement")?;
    let design = state
        .design
        .as_ref()
        .ok_or("This legacy workflow has no design lifecycle; start a goal-first workflow")?;
    let round = design
        .round
        .checked_add(1)
        .ok_or("planning round overflow")?;
    let task = design_task(
        round,
        format!(
            "Owner's decision about actual results and remaining work:\n{assessment}\nPrevious strategy: {}\nCompletion criteria: {:?}\nDeferred decisions: {:?}\nUse the actual dependency reports below, not the original proposal alone.",
            design.strategy, design.completion_criteria, design.deferred_decisions
        ),
        state
            .plan
            .tasks
            .iter()
            .filter(|t| t.kind == TaskKind::Work)
            .map(|t| t.id.clone())
            .collect(),
    );
    model::revise_tasks_with_removals(state, vec![task.clone()], &[])?;
    let design = state.design.as_mut().expect("design exists");
    design.round = round;
    design.assessment = None;
    Ok(task)
}

pub(super) fn finish(state: &mut WorkflowState, assessment: &str) -> Result<(), String> {
    if state.status != RunStatus::AwaitingDecision {
        return Err("finish requires awaiting_decision after all planned work has returned".into());
    }
    nonempty(
        assessment,
        "final assessment against the completion criteria",
    )?;
    state
        .design
        .as_mut()
        .ok_or("This legacy workflow has no design lifecycle")?
        .assessment = Some(assessment.into());
    state.status = RunStatus::Completed;
    Ok(())
}

pub(super) fn execution_succeeded(state: &WorkflowState) -> bool {
    if let Some(design) = &state.design {
        let planner = format!("_design_{}", design.round);
        state
            .tasks
            .get(&planner)
            .is_some_and(|task| task.status == TaskStatus::Completed)
            && state
                .tasks
                .values()
                .filter(|task| task.spec.depends_on.contains(&planner))
                .all(|task| task.status == TaskStatus::Completed)
    } else {
        state
            .tasks
            .values()
            .all(|task| task.status == TaskStatus::Completed)
    }
}

fn nonempty(text: &str, name: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.len() > 256 * 1024 {
        return Err(format!("{name} must be nonempty and at most 256 KiB"));
    }
    Ok(())
}

pub(super) fn validate_work(specs: &[TaskSpec], state: &WorkflowState) -> Result<(), String> {
    for task in specs {
        if task.kind != TaskKind::Work || task.id.starts_with("_design_") {
            return Err("Only the runtime can create workflow design tasks".into());
        }
        nonempty(
            task.deliverable.as_deref().unwrap_or(""),
            "task deliverable",
        )?;
        for dependency in &task.depends_on {
            if state
                .tasks
                .get(dependency)
                .is_some_and(|t| t.spec.kind == TaskKind::Design)
            {
                continue;
            }
            if !task.inputs.iter().any(|input| &input.task_id == dependency) {
                return Err(format!(
                    "Task {} must state how it uses dependency {dependency} in inputs",
                    task.id
                ));
            }
        }
    }
    Ok(())
}

/// Manual adjustments remain part of the current designed frontier and cannot
/// replace or bypass the native planning phase.
pub(super) fn prepare_revision(
    state: &WorkflowState,
    specs: &mut [TaskSpec],
    removals: &[String],
    goal_changed: bool,
    assessment: Option<&str>,
) -> Result<(), String> {
    let design = state
        .design
        .as_ref()
        .ok_or("Missing workflow design state")?;
    let planner = format!("_design_{}", design.round);
    if !state
        .tasks
        .get(&planner)
        .is_some_and(|task| task.status == TaskStatus::Completed)
    {
        return Err(
            "Wait for the current design or recover its failed attempt before revising work".into(),
        );
    }
    if removals.iter().any(|id| {
        state
            .tasks
            .get(id)
            .is_some_and(|task| task.spec.kind == TaskKind::Design)
    }) {
        return Err("Internal workflow design tasks cannot be removed".into());
    }
    validate_work(specs, state)?;
    for existing in state.tasks.values() {
        let drops_input = specs
            .iter()
            .find(|task| task.id == existing.spec.id)
            .is_some_and(|replacement| {
                existing.spec.inputs.iter().any(|input| {
                    !replacement
                        .inputs
                        .iter()
                        .any(|new_input| new_input.task_id == input.task_id)
                })
            });
        if (removals.contains(&existing.spec.id) || drops_input)
            && existing.spec.inputs.iter().any(|input| {
                state
                    .tasks
                    .get(&input.task_id)
                    .is_some_and(|prior| !model::is_terminal(prior.status))
            })
        {
            if !goal_changed {
                return Err(format!(
                    "Task {} consumes work that has not returned. Preserve the designed dependency; the scheduler will run this task when its inputs return. Final synthesis by the coordinator does not replace this planned work. For an actual scope change, revise goal and affected tasks together with an assessment identifying the changed objective or constraint.",
                    existing.spec.id
                ));
            }
            nonempty(
                assessment.unwrap_or(""),
                "assessment identifying the actual scope change",
            )?;
        }
    }
    for task in specs {
        if !task.depends_on.contains(&planner) {
            task.depends_on.push(planner.clone());
        }
    }
    Ok(())
}

/// A complete candidate is validated before changing the journal or admitting workers.
pub(super) fn apply_design(
    state: &mut WorkflowState,
    design_id: &str,
    output: &str,
) -> Result<Vec<TaskSpec>, String> {
    let result: DesignOutput = serde_json::from_str(output)
        .map_err(|e| format!("Planner returned an invalid structured design: {e}. Inspect the saved output and explicitly retry the planner."))?;
    nonempty(&result.strategy, "design strategy")?;
    if result.completion_criteria.is_empty() {
        return Err("The design must state practical completion criteria".into());
    }
    for criterion in &result.completion_criteria {
        nonempty(criterion, "completion criterion")?;
    }
    for decision in &result.deferred_decisions {
        nonempty(decision, "deferred decision")?;
    }
    if result.tasks.is_empty() && state.design.as_ref().is_some_and(|d| d.round == 1) {
        return Err(
            "The initial design must include useful work, not only a planning report".into(),
        );
    }
    let mut tasks = Vec::new();
    for task in result.tasks {
        if state.tasks.contains_key(&task.id) {
            return Err(format!(
                "Design reuses existing task {}; use a new ID and consume its report",
                task.id
            ));
        }
        if task
            .inputs
            .iter()
            .any(|input| input.task_id.starts_with("_design_"))
        {
            return Err(
                "Design inputs must reference work results, not internal planning tasks".into(),
            );
        }
        nonempty(&task.group, "task group")?;
        nonempty(&task.role, "task role")?;
        let mut depends_on = task
            .inputs
            .iter()
            .map(|input| input.task_id.clone())
            .collect::<Vec<_>>();
        // Retrying prerequisite work must invalidate plans and work based on its results.
        depends_on.push(design_id.into());
        tasks.push(TaskSpec {
            id: task.id,
            prompt: task.prompt,
            group: Some(task.group),
            role: Some(task.role),
            depends_on,
            run_after_failure: false,
            model: None,
            reasoning_effort: None,
            inherit_context: false,
            kind: TaskKind::Work,
            deliverable: Some(task.deliverable),
            inputs: task.inputs,
        });
    }
    validate_work(&tasks, state)?;
    let mut candidate = state.clone();
    model::revise_tasks_with_removals(&mut candidate, tasks.clone(), &[])?;
    let design = candidate
        .design
        .as_mut()
        .ok_or("Missing workflow design state")?;
    design.strategy = result.strategy;
    design.completion_criteria = result.completion_criteria;
    design.deferred_decisions = result.deferred_decisions;
    design.assessment = None;
    *state = candidate;
    Ok(tasks)
}

#[cfg(test)]
#[path = "planning_tests.rs"]
mod tests;
