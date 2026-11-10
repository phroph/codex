use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::write_models_cache;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnPlanStepStatus;
use codex_app_server_protocol::TurnPlanUpdatedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use codex_protocol::protocol::SubAgentSource;
use core_test_support::responses;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::matchers::method;
use wiremock::matchers::path;

const READ_TIMEOUT: Duration = Duration::from_secs(60);
const NAMESPACE: &str = "collaboration";
const START_CALL: &str = "workflow-start";
const STATUS_CALL: &str = "workflow-status";
const CANCEL_CALL: &str = "workflow-cancel";

fn is_root_request(request: &Request) -> bool {
    String::from_utf8_lossy(&request.body).contains(r#"\"agent_name\":\"/root\""#)
}

fn workflow_call(call_id: &str, arguments: Value) -> String {
    responses::sse(vec![
        responses::ev_response_created(call_id),
        responses::ev_function_call_with_namespace(
            call_id,
            NAMESPACE,
            "workflow",
            &arguments.to_string(),
        ),
        responses::ev_completed(call_id),
    ])
}

fn completed_response(id: &str, text: &str) -> String {
    responses::sse(vec![
        responses::ev_response_created(id),
        responses::ev_assistant_message(&format!("{id}-message"), text),
        responses::ev_completed(id),
    ])
}

fn tool_output_text(output: &Value) -> Option<String> {
    if let Some(text) = output.as_str() {
        return Some(text.to_string());
    }
    let content = output.as_array()?;
    Some(
        content
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn latest_workflow_result(body: &Value) -> Option<(String, Value)> {
    body["input"].as_array()?.iter().rev().find_map(|item| {
        let call_id = item["call_id"].as_str()?;
        if item["type"] != "function_call_output" || !call_id.starts_with("workflow-") {
            return None;
        }
        let text = tool_output_text(&item["output"])?;
        Some((
            call_id.to_string(),
            serde_json::from_str(&text).unwrap_or_else(|_| json!({"tool_error": text})),
        ))
    })
}

async fn fixture(server: &MockServer) -> Result<(TempDir, TestAppServer)> {
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_model("gpt-5.4")
        .enable_feature(Feature::WorkflowOrchestration)
        .with_extra_config(
            "[features.multi_agent_v2]\nenabled = true\nmax_concurrent_threads_per_session = 4\nroot_agent_usage_hint_text = \"Configured root role.\"",
        )
        .write(codex_home.path())?;
    write_models_cache(codex_home.path()).await?;
    let app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    Ok((codex_home, app_server))
}

async fn start_thread(app_server: &mut TestAppServer) -> Result<String> {
    let ThreadStartResponse { thread, .. } = app_server
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        })
        .await?;
    Ok(thread.id)
}

async fn run_turn(app_server: &mut TestAppServer, thread_id: &str, prompt: &str) -> Result<()> {
    let completed = timeout(
        READ_TIMEOUT,
        app_server.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    Ok(())
}

async fn worker(server: &MockServer, prompt: &'static str, output: &'static str) -> ResponseMock {
    responses::mount_sse_once_match(
        server,
        move |request: &Request| {
            let body = String::from_utf8_lossy(&request.body);
            body.contains(prompt) && !is_root_request(request)
        },
        completed_response(prompt, output),
    )
    .await
}

async fn workflow_plan(
    app_server: &mut TestAppServer,
    thread_id: &str,
    predicate: impl Fn(&TurnPlanUpdatedNotification) -> bool,
) -> Result<TurnPlanUpdatedNotification> {
    let notification = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_matching_notification(
            "scheduler plan update",
            |notification| {
                notification.method == "turn/plan/updated"
                    && notification.params.as_ref().is_some_and(|params| {
                        serde_json::from_value::<TurnPlanUpdatedNotification>(params.clone())
                            .is_ok_and(|plan| plan.thread_id == thread_id && predicate(&plan))
                    })
            },
        ),
    )
    .await??;
    Ok(serde_json::from_value(
        notification.params.context("workflow plan params")?,
    )?)
}

// ResponseMock records requests before later matchers run, so select this worker explicitly.
fn worker_requests(
    mock: &ResponseMock,
    prompt: &str,
    root_thread_id: &str,
) -> Vec<ResponsesRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| {
            request.header("thread-id").as_deref() != Some(root_thread_id)
                && request.body_contains_text(prompt)
        })
        .collect()
}

fn worker_request(mock: &ResponseMock, prompt: &str, root_thread_id: &str) -> ResponsesRequest {
    let requests = worker_requests(mock, prompt, root_thread_id);
    assert_eq!(requests.len(), 1, "expected one request from {prompt}");
    requests.into_iter().next().expect("one worker request")
}

async fn assert_native_child(
    app_server: &mut TestAppServer,
    root_thread_id: &str,
    request: &ResponsesRequest,
) -> Result<String> {
    let child_id = request
        .header("thread-id")
        .context("native child thread ID")?;
    let read: ThreadReadResponse = app_server
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: child_id.clone(),
                include_turns: false,
            },
        })
        .await?;
    assert_eq!(
        read.thread.parent_thread_id.as_deref(),
        Some(root_thread_id)
    );
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        agent_path: Some(agent_path),
        ..
    }) = &read.thread.source
    else {
        anyhow::bail!(
            "workflow worker is not a native task-path child: {:?}",
            read.thread.source
        );
    };
    assert_eq!(parent_thread_id.to_string(), root_thread_id);
    assert!(agent_path.as_str().starts_with("/root/"));
    assert!(!request.inputs_of_type("agent_message").is_empty());
    Ok(child_id)
}

/// The model plans once; native scheduling launches both branches and then the dependent task.
#[tokio::test]
async fn workflow_dispatches_parallel_branches_and_supplies_dependency_results() -> Result<()> {
    const LEFT_PROMPT: &str = "collect-left-workflow-evidence";
    const RIGHT_PROMPT: &str = "collect-right-workflow-evidence";
    const SYNTHESIS_PROMPT: &str = "synthesize-workflow-evidence";
    const LEFT_RESULT: &str = "LEFT_EVIDENCE: measured input is 17";
    const RIGHT_RESULT: &str = "RIGHT_EVIDENCE: measured output is 23";
    const SYNTHESIS_RESULT: &str = "Combined evidence inspected: 17 and 23";
    const WORKFLOW_ID: &str = "Native-evidence";

    let server = responses::start_mock_server().await;
    let left = worker(&server, LEFT_PROMPT, LEFT_RESULT).await;
    let right = worker(&server, RIGHT_PROMPT, RIGHT_RESULT).await;
    let synthesis = worker(&server, SYNTHESIS_PROMPT, SYNTHESIS_RESULT).await;
    let plan = json!({
        "id": WORKFLOW_ID,
        "goal": "Collect two independent measurements and combine their actual reports",
        "max_parallel": 2,
        "tasks": [
            {"id": "left-input", "prompt": LEFT_PROMPT, "group": "evidence", "role": "measure input"},
            {"id": "left_input", "prompt": RIGHT_PROMPT, "group": "evidence", "role": "measure output"},
            {"id": "combine", "prompt": SYNTHESIS_PROMPT, "depends_on": ["left-input", "left_input"], "role": "combine evidence"}
        ]
    });
    let root_requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&root_requests);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(is_root_request)
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("root request JSON");
            let mut requests = captured.lock().expect("root request log");
            requests.push(body.clone());
            let result = latest_workflow_result(&body);
            let response = match result {
                None => workflow_call(START_CALL, json!({"action": "start", "plan": plan})),
                Some((call_id, result)) if call_id == STATUS_CALL => {
                    completed_response("root-complete", &result.to_string())
                }
                Some((_, result)) if result["state"]["status"] == "completed" => workflow_call(
                    STATUS_CALL,
                    json!({"action": "status", "workflow_id": WORKFLOW_ID}),
                ),
                Some((_, result))
                    if result["state"]["status"] == "running" && requests.len() < 24 =>
                {
                    workflow_call(
                        &format!("workflow-wait-{}", requests.len()),
                        json!({
                            "action": "wait",
                            "workflow_id": WORKFLOW_ID,
                            "timeout_ms": 60000,
                            "after_revision": result["state"]["revision"],
                        }),
                    )
                }
                Some((_, result)) => {
                    completed_response("root-unexpected-result", &result.to_string())
                }
            };
            responses::sse_response(response)
        })
        .mount(&server)
        .await;

    let (_codex_home, mut app_server) = fixture(&server).await?;
    let root_thread_id = start_thread(&mut app_server).await?;
    run_turn(
        &mut app_server,
        &root_thread_id,
        "Use a workflow to delegate the two measurements and then combine their evidence.",
    )
    .await?;

    let captured = root_requests.lock().expect("root request log").clone();
    let (_, status) = latest_workflow_result(captured.last().context("final root request")?)
        .context("workflow result")?;
    assert_eq!(status["state"]["status"], "completed", "{captured:#?}");
    assert_eq!(status["state"]["plan"]["id"], WORKFLOW_ID);
    assert!(
        status["directory"]
            .as_str()
            .is_some_and(|path| !path.is_empty())
    );
    assert_eq!(
        status["state"]["tasks"]["combine"]["output"],
        SYNTHESIS_RESULT
    );
    assert!(
        captured[0]["input"]
            .to_string()
            .contains("Native workflow orchestration"),
        "workflow guidance must survive the configured root role override"
    );

    // No mock response calls update_plan: all these UI events come from the scheduler.
    let pending = workflow_plan(&mut app_server, &root_thread_id, |plan| {
        plan.plan.iter().any(|step| {
            step.step.contains("combine:")
                && step.step.contains("waiting for left-input, left_input")
        })
    })
    .await?;
    assert!(
        pending
            .plan
            .iter()
            .any(|step| step.step.contains("[evidence] left-input: measure input"))
    );
    assert!(
        pending
            .plan
            .iter()
            .all(|step| step.status == TurnPlanStepStatus::Pending)
    );
    workflow_plan(&mut app_server, &root_thread_id, |plan| {
        plan.plan
            .iter()
            .any(|step| step.status == TurnPlanStepStatus::InProgress)
    })
    .await?;
    let finished = workflow_plan(&mut app_server, &root_thread_id, |plan| {
        plan.explanation
            .as_deref()
            .is_some_and(|text| text.contains("completed: 3/3 reports returned"))
    })
    .await?;
    assert_eq!(finished.turn_id, pending.turn_id);
    assert_eq!(finished.plan.len(), 3);
    assert!(
        finished
            .plan
            .iter()
            .all(|step| step.status == TurnPlanStepStatus::Completed)
    );

    let child_requests = [
        worker_request(&left, LEFT_PROMPT, &root_thread_id),
        worker_request(&right, RIGHT_PROMPT, &root_thread_id),
        worker_request(&synthesis, SYNTHESIS_PROMPT, &root_thread_id),
    ];
    assert!(!child_requests[0].body_contains_text(RIGHT_RESULT));
    assert!(!child_requests[1].body_contains_text(LEFT_RESULT));
    assert!(child_requests[2].body_contains_text(LEFT_RESULT));
    assert!(child_requests[2].body_contains_text(RIGHT_RESULT));
    let mut child_ids = BTreeSet::new();
    for (task, request) in ["left-input", "left_input", "combine"]
        .into_iter()
        .zip(&child_requests)
    {
        let child_id = assert_native_child(&mut app_server, &root_thread_id, request).await?;
        assert_eq!(status["state"]["tasks"][task]["agent_id"], child_id);
        assert_eq!(status["state"]["tasks"][task]["status"], "completed");
        assert!(
            child_ids.insert(child_id),
            "each task must use its own native child"
        );
    }
    Ok(())
}

/// A late worker result resumes the idle coordinator without another user message.
#[tokio::test]
async fn completed_workflow_continues_idle_owner_once_with_saved_results() -> Result<()> {
    const WORKFLOW_ID: &str = "continue-evidence";
    const WORKER_PROMPT: &str = "collect-delayed-continuation-evidence";
    const WORKER_RESULT: &str = "Saved observation: the failure is caused by configuration drift";
    const SYNTHESIS_RESULT: &str =
        "I inspected the saved evidence and identified configuration drift.";

    let server = responses::start_mock_server().await;
    let delayed_worker = responses::mount_response_once_match(
        &server,
        |request: &Request| {
            String::from_utf8_lossy(&request.body).contains(WORKER_PROMPT)
                && !is_root_request(request)
        },
        responses::sse_response(completed_response("delayed-worker", WORKER_RESULT))
            .set_delay(Duration::from_secs(2)),
    )
    .await;
    let root_requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&root_requests);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(is_root_request)
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("root request JSON");
            let mut requests = captured.lock().expect("root request log");
            requests.push(body);
            let response = match requests.len() {
                1 => workflow_call(
                    START_CALL,
                    json!({
                        "action": "start",
                        "plan": {
                            "id": WORKFLOW_ID,
                            "goal": "Investigate the failure and deliver the diagnosis",
                            "tasks": [{"id": "evidence", "prompt": WORKER_PROMPT}]
                        }
                    }),
                ),
                2 => completed_response("root-dispatched", "Work dispatched"),
                3 => workflow_call(
                    STATUS_CALL,
                    json!({
                        "action": "status", "workflow_id": WORKFLOW_ID, "include_outputs": true
                    }),
                ),
                4 => completed_response("root-synthesis", SYNTHESIS_RESULT),
                _ => completed_response("unexpected-repeat", "Unexpected extra continuation"),
            };
            responses::sse_response(response)
        })
        .mount(&server)
        .await;

    let (_codex_home, mut app_server) = fixture(&server).await?;
    let root_thread_id = start_thread(&mut app_server).await?;
    // This is the only user turn submitted by the test.
    let first = timeout(
        READ_TIMEOUT,
        app_server.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: root_thread_id.clone(),
            input: vec![UserInput::Text {
                text: "Use a workflow to investigate the failure and deliver the diagnosis.".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;
    assert_eq!(first.turn.status, TurnStatus::Completed);

    let started = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_matching_notification(
            "automatic owner continuation",
            |notification| {
                notification.method == "turn/started"
                    && notification.params.as_ref().is_some_and(|params| {
                        serde_json::from_value::<TurnStartedNotification>(params.clone()).is_ok_and(
                            |started| {
                                started.thread_id == root_thread_id
                                    && started.turn.id != first.turn.id
                            },
                        )
                    })
            },
        ),
    )
    .await??;
    let started: TurnStartedNotification =
        serde_json::from_value(started.params.context("continued turn params")?)?;
    let completed = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_matching_notification(
            "automatic owner result",
            |notification| {
                notification.method == "turn/completed"
                    && notification.params.as_ref().is_some_and(|params| {
                        serde_json::from_value::<TurnCompletedNotification>(params.clone())
                            .is_ok_and(|completed| {
                                completed.thread_id == root_thread_id
                                    && completed.turn.id == started.turn.id
                            })
                    })
            },
        ),
    )
    .await??;
    let completed: TurnCompletedNotification =
        serde_json::from_value(completed.params.context("continued completion params")?)?;
    assert_eq!(completed.turn.status, TurnStatus::Completed);

    // Progress is retained for the original idle turn and republished when its
    // coordinator reads the workflow from the automatically continued turn.
    for turn_id in [&first.turn.id, &started.turn.id] {
        let plan = workflow_plan(&mut app_server, &root_thread_id, |plan| {
            &plan.turn_id == turn_id
                && plan
                    .explanation
                    .as_deref()
                    .is_some_and(|text| text.contains("completed: 1/1 reports returned"))
        })
        .await?;
        assert_eq!(plan.plan[0].status, TurnPlanStepStatus::Completed);
    }

    let requests = root_requests.lock().expect("root request log").clone();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2]["input"]
            .to_string()
            .contains("Native workflow continue-evidence")
    );
    let (call_id, result) =
        latest_workflow_result(&requests[3]).context("continued status result")?;
    assert_eq!(call_id, STATUS_CALL);
    assert_eq!(result["state"]["status"], "completed");
    assert_eq!(
        result["state"]["tasks"]["evidence"]["output"],
        WORKER_RESULT
    );
    let child_request = worker_request(&delayed_worker, WORKER_PROMPT, &root_thread_id);
    assert_native_child(&mut app_server, &root_thread_id, &child_request).await?;

    let repeated = timeout(
        Duration::from_secs(1),
        app_server.read_stream_until_matching_notification(
            "unexpected repeated owner continuation",
            |notification| {
                notification.method == "turn/started"
                    && notification.params.as_ref().is_some_and(|params| {
                        serde_json::from_value::<TurnStartedNotification>(params.clone()).is_ok_and(
                            |next| {
                                next.thread_id == root_thread_id
                                    && next.turn.id != first.turn.id
                                    && next.turn.id != started.turn.id
                            },
                        )
                    })
            },
        ),
    )
    .await;
    assert!(
        repeated.is_err(),
        "observed terminal status must not generate another owner turn"
    );
    assert_eq!(root_requests.lock().expect("root request log").len(), 4);
    let read: ThreadReadResponse = app_server
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: root_thread_id.clone(),
                include_turns: true,
            },
        })
        .await?;
    assert_eq!(read.thread.turns.len(), 2);
    assert!(serde_json::to_string(&read.thread.turns[1])?.contains(SYNTHESIS_RESULT));
    Ok(())
}

/// Cancellation is requested after a real worker has started, before its dependent can run.
#[tokio::test]
async fn cancelling_workflow_interrupts_active_work_and_cancels_pending_dependencies() -> Result<()>
{
    const WORKFLOW_ID: &str = "cancel-evidence";
    const SLOW_PROMPT: &str = "hold-workflow-worker-open";
    const DEPENDENT_PROMPT: &str = "never-dispatch-cancelled-dependent";
    const START_PROMPT: &str = "Start the cancellable workflow and report that it started.";
    const CANCEL_PROMPT: &str = "Cancel the active workflow now.";

    let server = responses::start_mock_server().await;
    let slow = responses::mount_response_once_match(
        &server,
        |request: &Request| {
            String::from_utf8_lossy(&request.body).contains(SLOW_PROMPT)
                && !is_root_request(request)
        },
        responses::sse_response(completed_response("slow-worker", "late worker output"))
            .set_delay(Duration::from_secs(120)),
    )
    .await;
    let dependent = worker(&server, DEPENDENT_PROMPT, "unexpected dependent execution").await;
    responses::mount_sse_once_match(
        &server,
        |request: &Request| {
            String::from_utf8_lossy(&request.body).contains(START_PROMPT)
                && is_root_request(request)
        },
        workflow_call(
            START_CALL,
            json!({
                "action": "start",
                "plan": {
                    "id": WORKFLOW_ID,
                    "goal": "Exercise cancellation while native child work is in flight",
                    "max_parallel": 1,
                    "tasks": [
                        {"id": "slow", "prompt": SLOW_PROMPT},
                        {"id": "dependent", "prompt": DEPENDENT_PROMPT, "depends_on": ["slow"]}
                    ]
                }
            }),
        ),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &Request| {
            String::from_utf8_lossy(&request.body).contains(START_CALL) && is_root_request(request)
        },
        completed_response("root-started", "Workflow started"),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &Request| {
            String::from_utf8_lossy(&request.body).contains(CANCEL_PROMPT)
                && is_root_request(request)
        },
        workflow_call(
            CANCEL_CALL,
            json!({"action": "cancel", "workflow_id": WORKFLOW_ID}),
        ),
    )
    .await;
    let cancelled = responses::mount_sse_once_match(
        &server,
        |request: &Request| {
            String::from_utf8_lossy(&request.body).contains(CANCEL_CALL) && is_root_request(request)
        },
        completed_response("root-cancelled", "Workflow cancelled"),
    )
    .await;

    let (_codex_home, mut app_server) = fixture(&server).await?;
    let root_thread_id = start_thread(&mut app_server).await?;
    run_turn(&mut app_server, &root_thread_id, START_PROMPT).await?;
    timeout(READ_TIMEOUT, async {
        while worker_requests(&slow, SLOW_PROMPT, &root_thread_id).is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    run_turn(&mut app_server, &root_thread_id, CANCEL_PROMPT).await?;

    let output = cancelled
        .function_call_output_text(CANCEL_CALL)
        .context("cancel result")?;
    let result: Value = serde_json::from_str(&output)?;
    assert_eq!(result["state"]["status"], "cancelled");
    assert_eq!(result["state"]["tasks"]["dependent"]["status"], "cancelled");
    let request = worker_request(&slow, SLOW_PROMPT, &root_thread_id);
    let child_id = assert_native_child(&mut app_server, &root_thread_id, &request).await?;
    timeout(READ_TIMEOUT, async {
        loop {
            let read: ThreadReadResponse = app_server
                .request(|request_id| ClientRequest::ThreadRead {
                    request_id,
                    params: ThreadReadParams {
                        thread_id: child_id.clone(),
                        include_turns: true,
                    },
                })
                .await?;
            if read
                .thread
                .turns
                .last()
                .is_some_and(|turn| turn.status == TurnStatus::Interrupted)
            {
                break Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;

    // Cancel stops admission immediately; a running task settles only after its native receipt.
    let journal_path =
        std::path::Path::new(result["directory"].as_str().context("workflow directory")?)
            .join("events.jsonl");
    let settled = timeout(READ_TIMEOUT, async {
        loop {
            let journal = tokio::fs::read(&journal_path).await?;
            // Skip any unfinished trailing append and inspect the latest committed record.
            let record = journal
                .rsplit(|byte| *byte == b'\n')
                .nth(1)
                .context("committed workflow event")?;
            let event: Value = serde_json::from_slice(record)?;
            if event["state"]["tasks"]["slow"]["status"] == "cancelled" {
                break Ok::<_, anyhow::Error>(event["state"].clone());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(settled["status"], "cancelled");
    assert_eq!(settled["tasks"]["dependent"]["status"], "cancelled");
    let cancelled_plan = workflow_plan(&mut app_server, &root_thread_id, |plan| {
        plan.explanation
            .as_deref()
            .is_some_and(|text| text.contains("— cancelled:"))
            && plan
                .plan
                .iter()
                .all(|step| step.step.contains("— cancelled"))
    })
    .await?;
    assert_eq!(cancelled_plan.plan.len(), 2);
    assert!(
        cancelled_plan
            .plan
            .iter()
            .all(|step| step.status == TurnPlanStepStatus::Pending)
    );
    assert!(worker_requests(&dependent, DEPENDENT_PROMPT, &root_thread_id).is_empty());
    Ok(())
}
