use super::*;
use crate::session::tests::make_session_and_context_with_rx;
use codex_protocol::AgentPath;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use tokio::sync::broadcast::error::TryRecvError;

#[tokio::test]
async fn workflow_outcomes_include_root_completion_and_root_or_child_interruptions() {
    let (session, mut turn, _events) = make_session_and_context_with_rx().await;
    let turn = Arc::get_mut(&mut turn).expect("turn context should not be shared");
    turn.multi_agent_version = MultiAgentVersion::V2;
    turn.session_source = SessionSource::Cli;
    let mut outcomes = session
        .services
        .agent_control
        .watch_workflow_outcomes()
        .expect("local controller exposes workflow outcomes");
    let completed = EventMsg::TurnComplete(TurnCompleteEvent {
        root_turn_id: None,
        turn_id: turn.sub_id.clone(),
        last_agent_message: Some("result".to_string()),
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    });

    session
        .maybe_notify_parent_of_terminal_turn(turn, &completed)
        .await;
    assert_eq!(
        outcomes.try_recv().expect("root completion is observed"),
        WorkflowAgentOutcome {
            thread_id: session.thread_id,
            turn_id: turn.sub_id.clone(),
            status: AgentStatus::Completed(Some("result".to_string())),
            idle: false,
        }
    );

    let interrupted = EventMsg::TurnAborted(TurnAbortedEvent {
        root_turn_id: None,
        turn_id: Some(turn.sub_id.clone()),
        reason: TurnAbortReason::Interrupted,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    });
    for source in [
        SessionSource::Cli,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_path: Some(AgentPath::root().join("worker").expect("valid path")),
            agent_nickname: None,
            agent_role: None,
        }),
    ] {
        turn.session_source = source;
        session
            .maybe_notify_parent_of_terminal_turn(turn, &interrupted)
            .await;
        assert_eq!(
            outcomes.try_recv().expect("interruption is observed"),
            WorkflowAgentOutcome {
                thread_id: session.thread_id,
                turn_id: turn.sub_id.clone(),
                status: AgentStatus::Interrupted,
                idle: false,
            }
        );
    }
    assert_eq!(outcomes.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn workflow_outcomes_include_raw_shutdown_without_duplicate_completion() {
    let (session, _turn, _events) = make_session_and_context_with_rx().await;
    let mut outcomes = session
        .services
        .agent_control
        .watch_workflow_outcomes()
        .expect("local controller exposes workflow outcomes");
    session
        .deliver_event_raw(Event {
            id: "shutdown-submission".to_string(),
            msg: EventMsg::ShutdownComplete,
        })
        .await;

    assert_eq!(
        outcomes.try_recv().expect("shutdown is observed"),
        WorkflowAgentOutcome {
            thread_id: session.thread_id,
            turn_id: "shutdown-submission".to_string(),
            status: AgentStatus::Shutdown,
            idle: false,
        }
    );
    assert_eq!(outcomes.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn workflow_idle_outcome_has_the_last_started_turn_after_reservation_release() {
    let (session, _turn, _events) = make_session_and_context_with_rx().await;
    let mut outcomes = session
        .services
        .agent_control
        .watch_workflow_outcomes()
        .unwrap();
    session.state.lock().await.last_started_turn_id = Some("prior-turn".into());
    session
        .agent_status
        .send_replace(AgentStatus::Completed(None));
    *session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
    session
        .emit_thread_idle_lifecycle_if_idle(codex_extension_api::ThreadIdleCause::Completed)
        .await;
    assert_eq!(outcomes.try_recv(), Err(TryRecvError::Empty));
    *session.active_turn.lock().await = None;
    session
        .emit_thread_idle_lifecycle_if_idle(codex_extension_api::ThreadIdleCause::Completed)
        .await;
    assert_eq!(
        outcomes.try_recv().unwrap(),
        WorkflowAgentOutcome {
            thread_id: session.thread_id,
            turn_id: "prior-turn".into(),
            status: AgentStatus::Completed(None),
            idle: true,
        }
    );
}
