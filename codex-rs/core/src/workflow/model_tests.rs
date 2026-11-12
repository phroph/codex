use super::*;
use pretty_assertions::assert_eq;

fn task(id: &str, dependencies: &[&str]) -> TaskSpec {
    TaskSpec {
        id: id.to_string(),
        prompt: format!("Perform {id}"),
        group: None,
        role: None,
        depends_on: dependencies.iter().map(|id| (*id).to_string()).collect(),
        run_after_failure: false,
        model: None,
        reasoning_effort: None,
        inherit_context: false,
        kind: TaskKind::Work,
        deliverable: None,
        inputs: Vec::new(),
    }
}

fn state(tasks: Vec<TaskSpec>) -> WorkflowState {
    new_state(
        WorkflowPlan {
            id: "example".to_string(),
            goal: "Complete the requested work".to_string(),
            tasks,
            max_parallel: 2,
        },
        "owner".to_string(),
    )
    .expect("valid workflow")
}

#[test]
fn dependency_scheduler_obeys_capacity_and_successful_predecessors() {
    let mut workflow = state(vec![
        task("a", &[]),
        task("b", &[]),
        task("c", &[]),
        task("verify", &["a", "b"]),
    ]);
    assert_eq!(ready_tasks(&workflow), vec!["a", "b"]);
    workflow.tasks.get_mut("a").unwrap().status = TaskStatus::Running;
    assert_eq!(ready_tasks(&workflow), vec!["b"]);
    workflow.tasks.get_mut("a").unwrap().status = TaskStatus::Completed;
    workflow.tasks.get_mut("b").unwrap().status = TaskStatus::Completed;
    assert_eq!(ready_tasks(&workflow), vec!["c", "verify"]);
    workflow.status = RunStatus::Paused;
    assert!(ready_tasks(&workflow).is_empty());
}

#[test]
fn cycles_unknown_dependencies_and_case_aliases_are_rejected() {
    let mut plan = state(vec![task("a", &[])]).plan;
    plan.tasks = vec![task("a", &["b"]), task("b", &["a"])];
    assert!(validate_plan(&plan).unwrap_err().contains("cycle"));
    plan.tasks = vec![task("a", &["missing"])];
    assert!(
        validate_plan(&plan)
            .unwrap_err()
            .contains("unknown dependency")
    );
    plan.tasks = vec![task("a", &[]), task("A", &[])];
    assert!(
        validate_plan(&plan)
            .unwrap_err()
            .contains("case-insensitive")
    );
    plan.tasks = vec![task("a", &["a"])];
    assert!(validate_plan(&plan).unwrap_err().contains("itself"));
}

#[test]
fn unsafe_ids_and_invalid_parallelism_are_rejected() {
    for id in ["..", "a/b", "a\\b", "a.b", "NUL", "COM1", "", "a:"] {
        assert!(validate_id(id, "id").is_err(), "accepted {id:?}");
    }
    let mut plan = state(vec![task("a", &[])]).plan;
    plan.max_parallel = 0;
    assert!(validate_plan(&plan).is_err());
    plan.max_parallel = MAX_PARALLEL + 1;
    assert!(validate_plan(&plan).is_err());
}

#[test]
fn failed_dependencies_block_descendants_but_failure_handlers_wait_for_every_dependency() {
    let mut cleanup = task("cleanup", &["a", "b"]);
    cleanup.run_after_failure = true;
    let mut workflow = state(vec![
        task("a", &[]),
        task("b", &[]),
        task("verify", &["a"]),
        task("critic", &["verify"]),
        cleanup,
    ]);
    workflow.tasks.get_mut("a").unwrap().status = TaskStatus::Failed;
    workflow.tasks.get_mut("b").unwrap().status = TaskStatus::Running;
    assert!(reconcile_blocked(&mut workflow));
    assert_eq!(workflow.tasks["verify"].status, TaskStatus::Blocked);
    assert_eq!(workflow.tasks["critic"].status, TaskStatus::Blocked);
    assert!(ready_tasks(&workflow).is_empty());
    workflow.tasks.get_mut("b").unwrap().status = TaskStatus::Interrupted;
    assert_eq!(ready_tasks(&workflow), vec!["cleanup"]);
    assert!(!reconcile_blocked(&mut workflow));
}

#[test]
fn revision_is_transactional_and_does_not_replace_started_results() {
    let mut workflow = state(vec![task("a", &[]), task("verify", &["a"])]);
    workflow.tasks.get_mut("a").unwrap().status = TaskStatus::Completed;
    workflow.tasks.get_mut("a").unwrap().output = Some("observed evidence".to_string());
    let before = workflow.clone();
    assert!(revise_tasks(&mut workflow, vec![task("new", &[]), task("a", &[])]).is_err());
    assert_eq!(workflow, before);
    assert!(
        revise_tasks(
            &mut workflow,
            vec![task("verify", &["new"]), task("new", &["verify"])]
        )
        .is_err()
    );
    assert_eq!(workflow, before);
    revise_tasks(
        &mut workflow,
        vec![task("verify", &["a", "new"]), task("new", &[])],
    )
    .expect("add and revise pending tasks");
    assert_eq!(workflow.tasks["a"], before.tasks["a"]);
    assert_eq!(ready_tasks(&workflow), vec!["new"]);
}

#[test]
fn obsolete_branch_removal_and_dependency_revision_are_atomic() {
    let mut workflow = state(vec![
        task("evidence", &[]),
        task("obsolete", &[]),
        task("synthesis", &["evidence", "obsolete"]),
    ]);
    let evidence = workflow.tasks.get_mut("evidence").unwrap();
    evidence.status = TaskStatus::Completed;
    evidence.attempt = 1;
    evidence.output = Some("measured evidence".to_string());
    let before = workflow.clone();
    assert!(
        revise_tasks_with_removals(&mut workflow, Vec::new(), &["obsolete".to_string()])
            .unwrap_err()
            .contains("unknown dependency obsolete")
    );
    assert_eq!(workflow, before);

    for status in [
        TaskStatus::Running,
        TaskStatus::Completed,
        TaskStatus::Failed,
        TaskStatus::Interrupted,
        TaskStatus::Cancelled,
        TaskStatus::Pending,
    ] {
        let mut attempted = before.clone();
        let obsolete = attempted.tasks.get_mut("obsolete").unwrap();
        obsolete.status = status;
        obsolete.attempt = 1;
        let unchanged = attempted.clone();
        assert!(
            revise_tasks_with_removals(
                &mut attempted,
                vec![task("synthesis", &["evidence"])],
                &["obsolete".to_string()],
            )
            .unwrap_err()
            .contains("only unstarted")
        );
        assert_eq!(attempted, unchanged);
    }

    revise_tasks_with_removals(
        &mut workflow,
        vec![task("synthesis", &["evidence"])],
        &["obsolete".to_string()],
    )
    .expect("remove obsolete work and rewire its dependent atomically");
    assert!(!workflow.tasks.contains_key("obsolete"));
    assert_eq!(workflow.plan.tasks.len(), 2);
    assert_eq!(workflow.tasks["evidence"], before.tasks["evidence"]);
    assert_eq!(ready_tasks(&workflow), vec!["synthesis"]);
    validate_state(&workflow).expect("plan and durable task state agree");
}

#[test]
fn retry_invalidates_transitive_outputs_and_retains_attempt_counts() {
    let mut workflow = state(vec![
        task("a", &[]),
        task("b", &["a"]),
        task("c", &["b"]),
        task("unrelated", &[]),
    ]);
    for task in workflow.tasks.values_mut() {
        task.status = TaskStatus::Completed;
        task.output = Some("previous result".to_string());
        task.agent_id = Some("previous-agent".to_string());
        task.attempt = 2;
    }
    let unaffected = workflow.tasks["unrelated"].clone();
    assert_eq!(
        reset_tasks(&mut workflow, &["a".to_string()]).unwrap(),
        vec!["a", "b", "c"]
    );
    for id in ["a", "b", "c"] {
        let task = &workflow.tasks[id];
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.attempt, 2);
        assert!(task.output.is_none());
        assert!(task.agent_id.is_none());
    }
    assert_eq!(workflow.tasks["unrelated"], unaffected);
}

#[test]
fn retry_is_rejected_atomically_if_any_descendant_is_running() {
    let mut workflow = state(vec![task("a", &[]), task("b", &["a"])]);
    workflow.tasks.get_mut("a").unwrap().status = TaskStatus::Completed;
    workflow.tasks.get_mut("b").unwrap().status = TaskStatus::Running;
    let before = workflow.clone();
    assert!(
        reset_tasks(&mut workflow, &["a".to_string()])
            .unwrap_err()
            .contains("b is running")
    );
    assert_eq!(workflow, before);
    assert!(reset_tasks(&mut workflow, &["missing".to_string()]).is_err());
    assert_eq!(workflow, before);
}

#[test]
fn restart_requires_attention_and_never_implicitly_retries_work() {
    let mut workflow = state(vec![
        task("done", &[]),
        task("active", &[]),
        task("later", &["active"]),
    ]);
    workflow.tasks.get_mut("done").unwrap().status = TaskStatus::Completed;
    workflow.tasks.get_mut("done").unwrap().output = Some("durable result".to_string());
    workflow.tasks.get_mut("active").unwrap().status = TaskStatus::Running;
    workflow.tasks.get_mut("active").unwrap().agent_id = Some("inspect-this-attempt".to_string());
    assert!(recover_after_restart(&mut workflow));
    assert_eq!(workflow.status, RunStatus::NeedsAttention);
    assert_eq!(workflow.tasks["active"].status, TaskStatus::Interrupted);
    assert_eq!(
        workflow.tasks["active"].agent_id.as_deref(),
        Some("inspect-this-attempt")
    );
    assert_eq!(workflow.tasks["later"].status, TaskStatus::Blocked);
    assert_eq!(
        workflow.tasks["done"].output.as_deref(),
        Some("durable result")
    );
    assert!(ready_tasks(&workflow).is_empty());
    assert!(!recover_after_restart(&mut workflow));
}

#[test]
fn minimal_json_uses_independent_context_and_parallel_defaults() {
    let plan: WorkflowPlan = serde_json::from_value(serde_json::json!({
        "id":"example", "goal":"Inspect source", "tasks":[{"id":"inspect","prompt":"Read source"}]
    }))
    .unwrap();
    assert_eq!(plan.max_parallel, 3);
    assert!(!plan.tasks[0].inherit_context);
    assert!(!plan.tasks[0].run_after_failure);
    assert!(plan.tasks[0].depends_on.is_empty());
}
