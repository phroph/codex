use crate::agent::LocalAgentControl;
use crate::agent::api::AgentControl;
use crate::agent::types::AgentMetadata;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;

fn control_with_limit(max_threads: usize) -> LocalAgentControl {
    let control = LocalAgentControl::default();
    control
        .runtime
        .agent_execution_limiter
        .initialize(max_threads);
    control
}

#[test]
fn execution_guards_count_active_v2_subagent_turns() {
    let control = control_with_limit(/*max_threads*/ 1);
    // Child role configs cannot replace the root-derived session limit.
    control
        .runtime
        .agent_execution_limiter
        .initialize(/*max_threads*/ 2);
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));

    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("first active turn should fit");
    let first = control
        .execution_guard(MultiAgentVersion::V2, &source)
        .expect("v2 subagent execution should be counted");
    let Err(err) = control.ensure_execution_capacity(MultiAgentVersion::V2, &source) else {
        panic!("second active turn should exceed the derived non-root cap");
    };
    let CodexErrorDetails::AgentLimitReached { max_threads } = err.details() else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*max_threads, 1);

    drop(first);
    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("capacity should be released when the running task drops");
}

#[test]
fn execution_guards_ignore_root_and_v1_turns() {
    let control = control_with_limit(/*max_threads*/ 0);

    assert!(
        control
            .execution_guard(MultiAgentVersion::V2, &SessionSource::Cli)
            .is_none()
    );
    assert!(
        control
            .execution_guard(
                MultiAgentVersion::V1,
                &SessionSource::SubAgent(SubAgentSource::Other("worker".to_string())),
            )
            .is_none()
    );
}

#[test]
fn workflow_guard_tracking_keeps_each_path_busy_until_its_last_guard_drops() {
    let control = control_with_limit(3);
    let path = AgentPath::root().join("worker").expect("valid path");
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 1,
        agent_path: Some(path.clone()),
        agent_nickname: None,
        agent_role: None,
    });
    let limiter = &control.runtime.agent_execution_limiter;
    let first = control.execution_guard(MultiAgentVersion::V2, &source);
    let second = control.execution_guard(MultiAgentVersion::V2, &source);
    assert!(limiter.has_active_path(&path));
    assert!(!limiter.has_active_path(&AgentPath::root().join("other").expect("valid path")));

    drop(first);
    assert!(limiter.has_active_path(&path));
    drop(second);
    assert!(!limiter.has_active_path(&path));
    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("native capacity is released with the same guards");
}

#[tokio::test]
async fn workflow_attempt_query_does_not_report_idle_while_a_guard_drains() {
    let control = control_with_limit(1);
    let agent_id = ThreadId::new();
    let path = AgentPath::root().join("worker").expect("valid path");
    let mut reservation = control
        .runtime
        .registry
        .reserve_spawn_slot(None)
        .expect("reserve test worker");
    reservation.reserve_agent_path(&path).expect("reserve path");
    reservation.commit(AgentMetadata {
        agent_id: Some(agent_id),
        agent_path: Some(path.clone()),
        ..AgentMetadata::default()
    });
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 1,
        agent_path: Some(path),
        agent_nickname: None,
        agent_role: None,
    });
    let guard = control.execution_guard(MultiAgentVersion::V2, &source);
    assert!(
        control
            .workflow_attempt_is_running(agent_id)
            .await
            .expect("a held execution guard proves native work is still active")
    );
    drop(guard);
    assert!(
        control.workflow_attempt_is_running(agent_id).await.is_err(),
        "an unavailable manager must not be mistaken for proof of an idle attempt"
    );
}
