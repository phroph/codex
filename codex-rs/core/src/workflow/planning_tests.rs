use super::*;
use pretty_assertions::assert_eq;

fn state() -> WorkflowState {
    start(
        "design-test".into(),
        "Diagnose the interacting failures and choose a fix".into(),
        2,
        None,
        "owner".into(),
    )
    .unwrap()
}

fn task(id: &str, inputs: Value) -> Value {
    json!({"id":id,"group":"Investigation","role":"Resolve a concrete question","prompt":format!("Perform {id}"),"deliverable":format!("Evidence from {id}"),"inputs":inputs})
}

fn output(tasks: Vec<Value>) -> String {
    json!({"strategy":"Establish causes before choosing a fix","completion_criteria":["Explain the interacting failures using original evidence"],"deferred_decisions":["Choose a fix after diagnosis"],"tasks":tasks}).to_string()
}

fn complete(state: &mut WorkflowState, id: &str, report: &str) {
    let task = state.tasks.get_mut(id).unwrap();
    task.status = TaskStatus::Completed;
    task.output = Some(report.into());
}

#[test]
fn workflow_design_start_does_not_dispatch_a_supplied_flat_proposal() {
    let proposal: WorkflowPlan = serde_json::from_value(json!({"id":"design-test","goal":"Investigate","tasks":[{"id":"eager","prompt":"Start immediately"}]})).unwrap();
    let workflow = start(
        proposal.id.clone(),
        proposal.goal.clone(),
        2,
        Some(&proposal),
        "owner".into(),
    )
    .unwrap();
    assert_eq!(model::ready_tasks(&workflow), vec!["_design_1"]);
    assert!(!workflow.tasks.contains_key("eager"));
    assert_eq!(workflow.tasks["_design_1"].spec.kind, TaskKind::Design);
    assert!(
        workflow.tasks["_design_1"]
            .spec
            .prompt
            .contains("proposal, not an instruction")
    );
}

#[test]
fn workflow_design_uses_declared_results_as_real_scheduling_dependencies() {
    let mut workflow = state();
    complete(&mut workflow, "_design_1", "design receipt");
    let design = output(vec![
        task("source", json!([])),
        task("recording", json!([])),
        task(
            "diagnosis",
            json!([
                {"task_id":"source","use_for":"Compare current implementation with the observed failure"},
                {"task_id":"recording","use_for":"Identify which proposed mechanism occurred"}
            ]),
        ),
    ]);
    apply_design(&mut workflow, "_design_1", &design).unwrap();
    assert_eq!(model::ready_tasks(&workflow), vec!["source", "recording"]);
    complete(&mut workflow, "source", "SOURCE: current implementation");
    assert_eq!(model::ready_tasks(&workflow), vec!["recording"]);
    complete(&mut workflow, "recording", "RECORDING: observed failure");
    assert_eq!(model::ready_tasks(&workflow), vec!["diagnosis"]);
    let prompt = super::super::task_prompt(&workflow, "diagnosis", std::path::Path::new("records"));
    assert!(prompt.contains("SOURCE: current implementation"));
    assert!(prompt.contains("RECORDING: observed failure"));
    assert!(prompt.contains("Identify which proposed mechanism occurred"));
    assert!(!prompt.contains("design receipt"));
}

#[test]
fn workflow_design_invalid_graph_is_atomic_and_not_a_fallback_to_flat_work() {
    let mut workflow = state();
    complete(&mut workflow, "_design_1", "raw result");
    let before = workflow.clone();
    let invalid = output(vec![task(
        "dependent",
        json!([{"task_id":"missing","use_for":"Need its evidence"}]),
    )]);
    assert!(
        apply_design(&mut workflow, "_design_1", &invalid)
            .unwrap_err()
            .contains("unknown dependency")
    );
    assert_eq!(workflow, before);
    assert!(apply_design(&mut workflow, "_design_1", "not JSON").is_err());
    assert_eq!(workflow, before);
}

#[test]
fn workflow_design_allows_independent_work_without_manufactured_review_stages() {
    let mut workflow = state();
    complete(&mut workflow, "_design_1", "design");
    apply_design(
        &mut workflow,
        "_design_1",
        &output(vec![
            task("independent-a", json!([])),
            task("independent-b", json!([])),
        ]),
    )
    .unwrap();
    assert_eq!(
        model::ready_tasks(&workflow),
        vec!["independent-a", "independent-b"]
    );
    assert!(workflow.tasks["independent-a"].spec.inputs.is_empty());
    assert_eq!(workflow.plan.tasks.len(), 3);
}

#[test]
fn workflow_design_finish_requires_a_coordinator_assessment_at_the_checkpoint() {
    let mut workflow = state();
    assert!(finish(&mut workflow, "done").is_err());
    complete(&mut workflow, "_design_1", "design");
    apply_design(
        &mut workflow,
        "_design_1",
        &output(vec![task("evidence", json!([]))]),
    )
    .unwrap();
    assert!(!execution_succeeded(&workflow));
    complete(&mut workflow, "evidence", "observed result");
    assert!(execution_succeeded(&workflow));
    workflow.status = RunStatus::AwaitingDecision;
    assert!(finish(&mut workflow, " ").is_err());
    finish(
        &mut workflow,
        "The source and recorded behavior establish the cause; the requested diagnosis is complete",
    )
    .unwrap();
    assert_eq!(workflow.status, RunStatus::Completed);
    assert!(
        workflow
            .design
            .unwrap()
            .assessment
            .unwrap()
            .contains("establish the cause")
    );
}

#[test]
fn workflow_design_advance_passes_actual_reports_and_defers_only_needed_work() {
    let mut workflow = state();
    complete(&mut workflow, "_design_1", "design");
    apply_design(
        &mut workflow,
        "_design_1",
        &output(vec![task("evidence", json!([]))]),
    )
    .unwrap();
    complete(
        &mut workflow,
        "evidence",
        "ACTUAL_RESULT: the magnetic reference is stale",
    );
    workflow.status = RunStatus::AwaitingDecision;
    let next = advance(
        &mut workflow,
        "Design a check of the newly identified stale-reference mechanism",
    )
    .unwrap();
    assert_eq!(next.id, "_design_2");
    assert_eq!(next.depends_on, vec!["evidence"]);
    let prompt = super::super::task_prompt(&workflow, &next.id, std::path::Path::new("records"));
    assert!(prompt.contains("ACTUAL_RESULT: the magnetic reference is stale"));
    assert!(prompt.contains("newly identified stale-reference mechanism"));
    complete(&mut workflow, "_design_2", "reassessment");
    apply_design(
        &mut workflow,
        "_design_2",
        &output(vec![task(
            "targeted-check",
            json!([{"task_id":"evidence","use_for":"Test the observed stale-reference mechanism"}]),
        )]),
    )
    .unwrap();
    workflow.status = RunStatus::Running;
    assert_eq!(model::ready_tasks(&workflow), vec!["targeted-check"]);
}

#[test]
fn workflow_design_does_not_replay_completed_planning_on_retry() {
    let mut workflow = state();
    complete(&mut workflow, "_design_1", "design");
    assert!(
        model::reset_tasks(&mut workflow, &["_design_1".into()])
            .unwrap_err()
            .contains("advance")
    );
}

#[test]
fn workflow_design_recovery_can_resolve_an_earlier_failed_frontier() {
    let mut workflow = state();
    complete(&mut workflow, "_design_1", "design");
    apply_design(
        &mut workflow,
        "_design_1",
        &output(vec![task("unavailable-evidence", json!([]))]),
    )
    .unwrap();
    workflow
        .tasks
        .get_mut("unavailable-evidence")
        .unwrap()
        .status = TaskStatus::Failed;
    workflow.status = RunStatus::NeedsAttention;
    advance(
        &mut workflow,
        "Use the alternative original evidence source to resolve the same question",
    )
    .unwrap();
    complete(&mut workflow, "_design_2", "new plan");
    apply_design(
        &mut workflow,
        "_design_2",
        &output(vec![task("alternative", json!([
            {"task_id":"unavailable-evidence","use_for":"Use its failure report to select an available original source"}
        ]))]),
    )
    .unwrap();
    workflow.status = RunStatus::Running;
    assert_eq!(model::ready_tasks(&workflow), vec!["alternative"]);
    complete(
        &mut workflow,
        "alternative",
        "resolved using independent original evidence",
    );
    assert!(execution_succeeded(&workflow));
    assert_eq!(
        workflow.tasks["unavailable-evidence"].status,
        TaskStatus::Failed
    );
}

#[test]
fn workflow_design_revisions_cannot_bypass_planning_or_hide_failed_work() {
    let mut workflow = state();
    let mut revisions: Vec<TaskSpec> = serde_json::from_value(json!([
        {"id":"follow-up","prompt":"Resolve the remaining question","deliverable":"Evidence for the answer"}
    ])).unwrap();
    assert!(prepare_revision(&workflow, &mut revisions, &[], false, None).is_err());
    complete(&mut workflow, "_design_1", "design");
    assert!(
        prepare_revision(
            &workflow,
            &mut revisions,
            &["_design_1".into()],
            false,
            None
        )
        .is_err()
    );
    prepare_revision(&workflow, &mut revisions, &[], false, None).unwrap();
    assert_eq!(revisions[0].depends_on, vec!["_design_1"]);
    model::revise_tasks_with_removals(&mut workflow, revisions, &[]).unwrap();
    workflow.tasks.get_mut("follow-up").unwrap().status = TaskStatus::Failed;
    assert!(!execution_succeeded(&workflow));
}

#[test]
fn workflow_design_consumer_cannot_be_flattened_before_its_inputs_return() {
    let mut workflow = state();
    complete(&mut workflow, "_design_1", "design");
    apply_design(&mut workflow, "_design_1", &output(vec![
        task("observation", json!([])),
        task("cross-check", json!([{"task_id":"observation","use_for":"Check the observed mechanism against source"}])),
    ])).unwrap();
    assert!(
        prepare_revision(&workflow, &mut [], &["cross-check".into()], false, None)
            .unwrap_err()
            .contains("has not returned")
    );
    let mut replacement = workflow.tasks["cross-check"].spec.clone();
    replacement.inputs.clear();
    replacement.depends_on = vec!["_design_1".into()];
    assert!(prepare_revision(&workflow, &mut [replacement], &[], false, None).is_err());
    // A changed objective may legitimately make the whole branch obsolete.
    prepare_revision(
        &workflow,
        &mut [],
        &["cross-check".into()],
        true,
        Some("The user withdrew the mechanism check from scope"),
    )
    .unwrap();
    complete(
        &mut workflow,
        "observation",
        "actual evidence permits reassessment",
    );
    prepare_revision(&workflow, &mut [], &["cross-check".into()], false, None).unwrap();
}
