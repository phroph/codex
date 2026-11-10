use super::*;
use crate::workflow::model::RunStatus;
use crate::workflow::model::TaskStatus;
use crate::workflow::model::WorkflowPlan;
use crate::workflow::model::new_state;
use crate::workflow::model::recover_after_restart;
use crate::workflow::model::reset_tasks;
use pretty_assertions::assert_eq;

fn state() -> WorkflowState {
    let plan: WorkflowPlan = serde_json::from_value(serde_json::json!({
        "id":"test-workflow", "goal":"Inspect and verify", "tasks":[
            {"id":"inspect","prompt":"Inspect the source"},
            {"id":"verify","prompt":"Verify the source", "depends_on":["inspect"]}
        ]
    }))
    .expect("valid plan fixture");
    new_state(plan, "owner-thread".to_string()).unwrap()
}

#[tokio::test]
async fn journal_roundtrip_preserves_results_and_materializes_outputs() {
    let home = tempfile::tempdir().unwrap();
    let store = WorkflowStore::new(home.path(), "owner-thread", "test-workflow").unwrap();
    assert!(store.load().await.unwrap().is_none());
    let mut workflow = state();
    store.save(&mut workflow, "created").await.unwrap();
    assert_eq!(workflow.revision, 1);
    workflow.tasks.get_mut("inspect").unwrap().status = TaskStatus::Completed;
    workflow.tasks.get_mut("inspect").unwrap().output =
        Some("Evidence: observed source".to_string());
    store.save(&mut workflow, "task_completed").await.unwrap();
    assert_eq!(workflow.revision, 2);
    assert_eq!(store.load().await.unwrap(), Some(workflow.clone()));
    assert_eq!(
        std::fs::read_to_string(store.directory().join("outputs/inspect.txt")).unwrap(),
        "Evidence: observed source"
    );
    let snapshot: WorkflowState =
        serde_json::from_slice(&std::fs::read(store.directory().join("state.json")).unwrap())
            .unwrap();
    assert_eq!(snapshot, workflow);
    assert_eq!(
        std::fs::read_to_string(store.directory().join("events.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[tokio::test]
async fn stale_revision_cannot_overwrite_a_newer_commit() {
    let home = tempfile::tempdir().unwrap();
    let store = WorkflowStore::new(home.path(), "owner-thread", "test-workflow").unwrap();
    let mut first = state();
    store.save(&mut first, "created").await.unwrap();
    let mut stale = first.clone();
    first.status = RunStatus::Paused;
    store.save(&mut first, "paused").await.unwrap();
    stale.status = RunStatus::Cancelled;
    let error = store.save(&mut stale, "cancelled").await.unwrap_err();
    assert!(error.contains("revision changed"));
    assert_eq!(stale.revision, 1);
    assert_eq!(store.load().await.unwrap(), Some(first));
}

#[tokio::test]
async fn corrupt_snapshot_does_not_hide_newer_journal_state() {
    let home = tempfile::tempdir().unwrap();
    let store = WorkflowStore::new(home.path(), "owner-thread", "test-workflow").unwrap();
    let mut workflow = state();
    store.save(&mut workflow, "created").await.unwrap();
    std::fs::write(store.directory().join("state.json"), b"partial snapshot").unwrap();
    assert_eq!(store.load().await.unwrap(), Some(workflow));
}

#[tokio::test]
async fn incomplete_last_record_is_ignored_then_replaced_without_losing_commits() {
    let home = tempfile::tempdir().unwrap();
    let store = WorkflowStore::new(home.path(), "owner-thread", "test-workflow").unwrap();
    let mut workflow = state();
    store.save(&mut workflow, "created").await.unwrap();
    let journal_path = store.directory().join("events.jsonl");
    let committed = std::fs::read(&journal_path).unwrap();
    OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .unwrap()
        .write_all(b"{\"revision\":2,")
        .unwrap();
    assert_eq!(store.load().await.unwrap(), Some(workflow.clone()));
    workflow.status = RunStatus::Paused;
    store.save(&mut workflow, "paused").await.unwrap();
    assert_eq!(workflow.revision, 2);
    assert_eq!(store.load().await.unwrap(), Some(workflow));
    assert!(
        std::fs::read(&journal_path)
            .unwrap()
            .starts_with(&committed)
    );
}

#[tokio::test]
async fn malformed_committed_record_fails_closed() {
    let home = tempfile::tempdir().unwrap();
    let store = WorkflowStore::new(home.path(), "owner-thread", "test-workflow").unwrap();
    let mut workflow = state();
    store.save(&mut workflow, "created").await.unwrap();
    OpenOptions::new()
        .append(true)
        .open(store.directory().join("events.jsonl"))
        .unwrap()
        .write_all(b"corrupt committed record\n")
        .unwrap();
    assert!(
        store
            .load()
            .await
            .unwrap_err()
            .contains("invalid committed workflow event")
    );
    assert!(store.save(&mut workflow, "update").await.is_err());
    assert_eq!(workflow.revision, 1);
}

#[tokio::test]
async fn restart_recovery_is_explicit_and_durable_without_rerunning_tasks() {
    let home = tempfile::tempdir().unwrap();
    let store = WorkflowStore::new(home.path(), "owner-thread", "test-workflow").unwrap();
    let mut workflow = state();
    workflow.tasks.get_mut("inspect").unwrap().status = TaskStatus::Running;
    workflow.tasks.get_mut("inspect").unwrap().attempt = 1;
    workflow.tasks.get_mut("inspect").unwrap().agent_id = Some("previous-attempt".to_string());
    store.save(&mut workflow, "task_started").await.unwrap();
    let fresh_store = WorkflowStore::new(home.path(), "owner-thread", "test-workflow").unwrap();
    let mut recovered = fresh_store.load().await.unwrap().unwrap();
    assert_eq!(recovered.tasks["inspect"].status, TaskStatus::Running);
    assert!(recover_after_restart(&mut recovered));
    fresh_store.save(&mut recovered, "recovered").await.unwrap();
    let reloaded = fresh_store.load().await.unwrap().unwrap();
    assert_eq!(reloaded.status, RunStatus::NeedsAttention);
    assert_eq!(reloaded.tasks["inspect"].status, TaskStatus::Interrupted);
    assert_eq!(reloaded.tasks["inspect"].attempt, 1);
    assert_eq!(
        reloaded.tasks["inspect"].agent_id.as_deref(),
        Some("previous-attempt")
    );
}

#[tokio::test]
async fn explicit_retry_clears_materialized_stale_outputs_but_retains_event_history() {
    let home = tempfile::tempdir().unwrap();
    let store = WorkflowStore::new(home.path(), "owner-thread", "test-workflow").unwrap();
    let mut workflow = state();
    for task in workflow.tasks.values_mut() {
        task.status = TaskStatus::Completed;
        task.output = Some("old output".to_string());
    }
    store.save(&mut workflow, "finished").await.unwrap();
    reset_tasks(&mut workflow, &["inspect".to_string()]).unwrap();
    store.save(&mut workflow, "retry_requested").await.unwrap();
    assert!(!store.directory().join("outputs/inspect.txt").exists());
    assert!(!store.directory().join("outputs/verify.txt").exists());
    assert!(
        std::fs::read_to_string(store.directory().join("events.jsonl"))
            .unwrap()
            .contains("old output")
    );
}

#[tokio::test]
async fn identity_mismatch_and_invalid_stored_graph_are_rejected() {
    let home = tempfile::tempdir().unwrap();
    let store = WorkflowStore::new(home.path(), "another-owner", "test-workflow").unwrap();
    let mut workflow = state();
    assert!(
        store
            .save(&mut workflow, "created")
            .await
            .unwrap_err()
            .contains("identity")
    );
    let store = WorkflowStore::new(home.path(), "owner-thread", "test-workflow").unwrap();
    workflow.tasks.remove("verify");
    assert!(
        store
            .save(&mut workflow, "created")
            .await
            .unwrap_err()
            .contains("task count")
    );
    assert!(WorkflowStore::new(home.path(), "../other", "test-workflow").is_err());
}
