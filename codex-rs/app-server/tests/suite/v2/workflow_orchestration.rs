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
const NAMESPACE: &str = "workflows";
const START_CALL: &str = "workflow-start";
const STATUS_CALL: &str = "workflow-status";
const CANCEL_CALL: &str = "workflow-cancel";
const FINISH_CALL: &str = "workflow-finish";
const ADVANCE_CALL: &str = "workflow-advance";

fn assigned_task(body: &Value) -> Option<String> {
    body["input"].as_array()?.iter().rev().find_map(|item| {
        if item["type"] != "agent_message" {
            return None;
        }
        item["content"].as_array()?.iter().find_map(|span| {
            let text = span["text"].as_str()?;
            text.contains("You are executing task '")
                .then(|| text.to_string())
        })
    })
}

fn assigned_prompt(body: &Value, prompt: &str) -> bool {
    assigned_task(body).is_some_and(|text| text.contains(&format!("\nTask: {prompt}\n")))
}

fn is_root_request(request: &Request) -> bool {
    let body: Value = serde_json::from_slice(&request.body).expect("Responses request JSON");
    assigned_task(&body).is_none()
        && String::from_utf8_lossy(&request.body).contains(r#"\"agent_name\":\"/root\""#)
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

fn assert_workflow_catalog_uses_custom_namespace(body: &Value) {
    let declarations = body["tools"].as_array().into_iter().flatten().chain(
        body["input"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "additional_tools")
            .flat_map(|item| item["tools"].as_array().into_iter().flatten()),
    );
    let workflow_namespaces = declarations
        .filter(|namespace| {
            namespace["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "workflow"))
        })
        .map(|namespace| namespace["name"].as_str().expect("namespace name"))
        .collect::<Vec<_>>();
    assert_eq!(workflow_namespaces, vec!["workflows"]);
}

async fn fixture(server: &MockServer) -> Result<(TempDir, TestAppServer)> {
    fixture_at_url(&server.uri()).await
}

async fn fixture_at_url(url: &str) -> Result<(TempDir, TestAppServer)> {
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(url)
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
            assigned_prompt(
                &serde_json::from_slice(&request.body).expect("request JSON"),
                prompt,
            )
        },
        completed_response(prompt, output),
    )
    .await
}

fn design(tasks: Value) -> Value {
    json!({
        "strategy": "Collect the necessary observations, then interpret the actual evidence",
        "completion_criteria": ["The conclusion cites the returned observations"],
        "deferred_decisions": [],
        "tasks": tasks,
    })
}

async fn planner(
    server: &MockServer,
    workflow_id: &'static str,
    round: u32,
    output: Value,
) -> ResponseMock {
    let assignment =
        format!("You are executing task '_design_{round}' in native workflow '{workflow_id}'.");
    responses::mount_sse_once_match(
        server,
        move |request: &Request| {
            assigned_task(&serde_json::from_slice(&request.body).expect("request JSON"))
                .is_some_and(|text| text.contains(&assignment))
        },
        completed_response(&format!("design-{round}"), &output.to_string()),
    )
    .await
}

fn planner_request(mock: &ResponseMock, round: u32) -> ResponsesRequest {
    let assignment = format!("You are executing task '_design_{round}'");
    let requests = mock
        .requests()
        .into_iter()
        .filter(|request| {
            assigned_task(&request.body_json()).is_some_and(|text| text.contains(&assignment))
        })
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 1, "one native design request per round");
    requests.into_iter().next().expect("planner request")
}

fn assert_planner_contract(request: &ResponsesRequest) {
    let body = request.body_json();
    let text = json!({"type": "string"});
    let strings = json!({"type": "array", "items": text});
    let schema = json!({
        "type": "object", "additionalProperties": false,
        "properties": {
            "strategy": text,
            "completion_criteria": strings,
            "deferred_decisions": strings,
            "tasks": {"type": "array", "items": {
                "type": "object", "additionalProperties": false,
                "properties": {
                    "id": text, "group": text, "role": text, "prompt": text,
                    "deliverable": text,
                    "inputs": {"type": "array", "items": {
                        "type": "object", "additionalProperties": false,
                        "properties": {"task_id": text, "use_for": text},
                        "required": ["task_id", "use_for"]
                    }}
                },
                "required": ["id", "group", "role", "prompt", "deliverable", "inputs"]
            }}
        },
        "required": ["strategy", "completion_criteria", "deferred_decisions", "tasks"]
    });
    assert_eq!(
        body["text"]["format"],
        json!({
            "name": "codex_output_schema", "type": "json_schema", "strict": true, "schema": schema
        })
    );
    let declarations = body["tools"].as_array().into_iter().flatten().chain(
        body["input"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "additional_tools")
            .flat_map(|item| item["tools"].as_array().into_iter().flatten()),
    );
    for namespace in declarations {
        for tool in namespace["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(std::iter::once(namespace))
        {
            assert!(
                !matches!(
                    tool["name"].as_str(),
                    Some(
                        "workflow"
                            | "spawn_agent"
                            | "send_message"
                            | "followup_task"
                            | "wait_agent"
                            | "interrupt_agent"
                            | "list_agents"
                    )
                ),
                "planner must not recursively dispatch work: {tool}"
            );
        }
    }
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
                && assigned_prompt(&request.body_json(), prompt)
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

/// A native designer expands the caller's proposal, then scheduling enforces its information flow.
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
    let designer = planner(&server, WORKFLOW_ID, 1, design(json!([
        {"id": "left-input", "prompt": LEFT_PROMPT, "group": "evidence", "role": "measure input", "deliverable": "Measured input", "inputs": []},
        {"id": "left_input", "prompt": RIGHT_PROMPT, "group": "evidence", "role": "measure output", "deliverable": "Measured output", "inputs": []},
        {"id": "combine", "prompt": SYNTHESIS_PROMPT, "group": "interpretation", "role": "combine evidence", "deliverable": "Conclusion from both measurements", "inputs": [
            {"task_id": "left-input", "use_for": "Interpret the measured input"},
            {"task_id": "left_input", "use_for": "Compare the measured output with the input"}
        ]}
    ]))).await;
    let plan = json!({
        "id": WORKFLOW_ID,
        "goal": "Collect two independent measurements and combine their actual reports",
        "max_parallel": 2,
        "tasks": [
            {"id": "left-input", "prompt": LEFT_PROMPT, "group": "evidence", "role": "measure input"},
            {"id": "left_input", "prompt": RIGHT_PROMPT, "group": "evidence", "role": "measure output"}
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
                    json!({"action": "status", "workflow_id": WORKFLOW_ID, "include_outputs": true}),
                ),
                Some((_, result)) if result["state"]["status"] == "awaiting_decision" => workflow_call(
                    FINISH_CALL,
                    json!({"action": "finish", "workflow_id": WORKFLOW_ID, "assessment": "Both measurements and the combined report support the requested conclusion."}),
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
    assert_workflow_catalog_uses_custom_namespace(&captured[0]);
    let (_, status) = latest_workflow_result(captured.last().context("final root request")?)
        .context("workflow result")?;
    assert_eq!(status["state"]["status"], "completed", "{captured:#?}");
    assert_eq!(status["state"]["plan"]["id"], WORKFLOW_ID);
    assert_eq!(status["state"]["design"]["round"], 1);
    assert_eq!(
        status["state"]["tasks"]["_design_1"]["spec"]["kind"],
        "design"
    );
    assert_eq!(
        status["state"]["tasks"]["combine"]["spec"]["inputs"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    assert!(captured.iter().any(|body| {
        latest_workflow_result(body)
            .is_some_and(|(_, result)| result["state"]["status"] == "awaiting_decision")
    }));
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
            .contains("Native workflow design and execution"),
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
            .filter(|step| !step.step.contains("_design_1:"))
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
            .is_some_and(|text| text.contains("completed: 4/4 reports returned"))
    })
    .await?;
    assert_eq!(finished.turn_id, pending.turn_id);
    assert_eq!(finished.plan.len(), 4);
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
    let design_request = planner_request(&designer, 1);
    assert_planner_contract(&design_request);
    let design_child =
        assert_native_child(&mut app_server, &root_thread_id, &design_request).await?;
    assert_eq!(
        status["state"]["tasks"]["_design_1"]["agent_id"],
        design_child
    );
    assert!(!child_requests[0].body_contains_text(RIGHT_RESULT));
    assert!(!child_requests[1].body_contains_text(LEFT_RESULT));
    assert!(child_requests[2].body_contains_text(LEFT_RESULT));
    assert!(child_requests[2].body_contains_text(RIGHT_RESULT));
    let mut child_ids = BTreeSet::new();
    child_ids.insert(design_child);
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
async fn returned_workflow_continues_idle_owner_once_for_coordinator_acceptance() -> Result<()> {
    const WORKFLOW_ID: &str = "continue-evidence";
    const WORKER_PROMPT: &str = "collect-delayed-continuation-evidence";
    const WORKER_RESULT: &str = "Saved observation: the failure is caused by configuration drift";
    const SYNTHESIS_RESULT: &str =
        "I inspected the saved evidence and identified configuration drift.";

    let server = responses::start_mock_server().await;
    let designer = planner(&server, WORKFLOW_ID, 1, design(json!([
        {"id": "evidence", "prompt": WORKER_PROMPT, "group": "diagnosis", "role": "collect cause evidence", "deliverable": "Observed cause", "inputs": []}
    ]))).await;
    let delayed_worker = responses::mount_response_once_match(
        &server,
        |request: &Request| {
            assigned_prompt(
                &serde_json::from_slice(&request.body).expect("request JSON"),
                WORKER_PROMPT,
            )
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
                        "workflow_id": WORKFLOW_ID,
                        "goal": "Investigate the failure and deliver the diagnosis"
                    }),
                ),
                2 => completed_response("root-dispatched", "Work dispatched"),
                3 => workflow_call(
                    STATUS_CALL,
                    json!({
                        "action": "status", "workflow_id": WORKFLOW_ID, "include_outputs": true
                    }),
                ),
                4 => workflow_call(FINISH_CALL, json!({
                    "action": "finish", "workflow_id": WORKFLOW_ID, "assessment": SYNTHESIS_RESULT
                })),
                5 => completed_response("root-synthesis", SYNTHESIS_RESULT),
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
    for (turn_id, expected_status) in [
        (
            &first.turn.id,
            "awaiting coordinator decision: 2/2 reports returned",
        ),
        (&started.turn.id, "completed: 2/2 reports returned"),
    ] {
        let plan = workflow_plan(&mut app_server, &root_thread_id, |plan| {
            &plan.turn_id == turn_id
                && plan
                    .explanation
                    .as_deref()
                    .is_some_and(|text| text.contains(expected_status))
        })
        .await?;
        assert_eq!(plan.plan[0].status, TurnPlanStepStatus::Completed);
    }

    let requests = root_requests.lock().expect("root request log").clone();
    assert_eq!(requests.len(), 5);
    assert!(
        requests[2]["input"]
            .to_string()
            .contains("Native workflow continue-evidence")
    );
    let (call_id, result) =
        latest_workflow_result(&requests[3]).context("continued status result")?;
    assert_eq!(call_id, STATUS_CALL);
    assert_eq!(result["state"]["status"], "awaiting_decision");
    assert_eq!(
        result["state"]["tasks"]["evidence"]["output"],
        WORKER_RESULT
    );
    let child_request = worker_request(&delayed_worker, WORKER_PROMPT, &root_thread_id);
    assert_native_child(&mut app_server, &root_thread_id, &child_request).await?;
    assert_native_child(
        &mut app_server,
        &root_thread_id,
        &planner_request(&designer, 1),
    )
    .await?;
    let (call_id, accepted) = latest_workflow_result(&requests[4]).context("finish result")?;
    assert_eq!(call_id, FINISH_CALL);
    assert_eq!(accepted["state"]["status"], "completed");
    assert_eq!(accepted["state"]["design"]["assessment"], SYNTHESIS_RESULT);

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
    assert_eq!(root_requests.lock().expect("root request log").len(), 5);
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
    let designer = planner(&server, WORKFLOW_ID, 1, design(json!([
        {"id": "slow", "prompt": SLOW_PROMPT, "group": "evidence", "role": "collect slow observation", "deliverable": "Observation", "inputs": []},
        {"id": "dependent", "prompt": DEPENDENT_PROMPT, "group": "interpretation", "role": "interpret observation", "deliverable": "Interpretation", "inputs": [{"task_id": "slow", "use_for": "Interpret the observed result"}]}
    ]))).await;
    let slow = responses::mount_response_once_match(
        &server,
        |request: &Request| {
            assigned_prompt(
                &serde_json::from_slice(&request.body).expect("request JSON"),
                SLOW_PROMPT,
            )
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
    assert_eq!(result["state"]["tasks"]["_design_1"]["status"], "completed");
    assert_native_child(
        &mut app_server,
        &root_thread_id,
        &planner_request(&designer, 1),
    )
    .await?;
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
                .filter(|step| !step.step.contains("_design_1:"))
                .all(|step| step.step.contains("— cancelled"))
    })
    .await?;
    assert_eq!(cancelled_plan.plan.len(), 3);
    assert!(
        cancelled_plan
            .plan
            .iter()
            .filter(|step| !step.step.contains("_design_1:"))
            .all(|step| step.status == TurnPlanStepStatus::Pending)
    );
    assert!(worker_requests(&dependent, DEPENDENT_PROMPT, &root_thread_id).is_empty());
    Ok(())
}

/// The next frontier is designed from an actual result and the coordinator's decision.
#[tokio::test]
async fn workflow_advances_from_returned_evidence_before_explicit_finish() -> Result<()> {
    const WORKFLOW_ID: &str = "adaptive-evidence";
    const BASELINE_PROMPT: &str = "identify-which-startup-path-fails";
    const BASELINE_RESULT: &str =
        "Trace evidence: failure occurs only after resume; cold starts pass.";
    const FOLLOWUP_PROMPT: &str = "check-the-observed-resume-path";
    const FOLLOWUP_RESULT: &str = "Resume reuses stale settings; the cold-start path reloads them.";

    let server = responses::start_mock_server().await;
    let mut initial_design = design(json!([
        {"id": "baseline", "prompt": BASELINE_PROMPT, "group": "discovery", "role": "identify affected path", "deliverable": "Observed failing and passing paths", "inputs": []}
    ]));
    initial_design["deferred_decisions"] =
        json!(["Choose which startup path to inspect from the actual trace."]);
    let first_designer = planner(&server, WORKFLOW_ID, 1, initial_design).await;
    let second_designer = planner(&server, WORKFLOW_ID, 2, design(json!([
        {"id": "resume-path", "prompt": FOLLOWUP_PROMPT, "group": "diagnosis", "role": "explain the failing path", "deliverable": "Cause of resume-only failure", "inputs": [{"task_id": "baseline", "use_for": "Focus on the observed failing resume path and compare with the passing cold start"}]}
    ]))).await;
    let baseline = worker(&server, BASELINE_PROMPT, BASELINE_RESULT).await;
    let followup = worker(&server, FOLLOWUP_PROMPT, FOLLOWUP_RESULT).await;
    let root_requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&root_requests);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(is_root_request)
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("root request JSON");
            let mut requests = captured.lock().expect("root request log");
            requests.push(body.clone());
            let response = match latest_workflow_result(&body) {
                None => workflow_call(START_CALL, json!({
                    "action": "start", "workflow_id": WORKFLOW_ID,
                    "goal": "Diagnose the startup failure using the observed failing path"
                })),
                Some((_, result)) if result["state"]["status"] == "awaiting_decision"
                    && result["state"]["design"]["round"] == 1 => {
                    let observation = result["state"]["tasks"]["baseline"]["output"].as_str().expect("returned baseline");
                    workflow_call(ADVANCE_CALL, json!({
                        "action": "advance", "workflow_id": WORKFLOW_ID,
                        "assessment": format!("The returned evidence is: {observation} Inspect the resume-specific path to explain this difference.")
                    }))
                }
                Some((_, result)) if result["state"]["status"] == "awaiting_decision" => {
                    assert_eq!(result["state"]["tasks"]["resume-path"]["output"], FOLLOWUP_RESULT);
                    workflow_call(FINISH_CALL, json!({
                        "action": "finish", "workflow_id": WORKFLOW_ID,
                        "assessment": "The observed resume-only failure and stale-setting cause answer the objective.",
                        "include_outputs": true
                    }))
                }
                Some((_, result)) if result["state"]["status"] == "running" && requests.len() < 32 => workflow_call(
                    &format!("workflow-wait-{}", requests.len()),
                    json!({"action": "wait", "workflow_id": WORKFLOW_ID, "timeout_ms": 60000, "after_revision": result["state"]["revision"], "include_outputs": true}),
                ),
                Some((_, result)) => completed_response("root-adaptive-result", &result.to_string()),
            };
            responses::sse_response(response)
        })
        .mount(&server)
        .await;

    let (_codex_home, mut app_server) = fixture(&server).await?;
    let root_thread_id = start_thread(&mut app_server).await?;
    run_turn(&mut app_server, &root_thread_id, "Diagnose the startup failure and use the observations to choose the useful next investigation.").await?;

    let requests = root_requests.lock().expect("root request log").clone();
    let (call_id, result) = latest_workflow_result(requests.last().context("final request")?)
        .context("final workflow result")?;
    assert_eq!(call_id, FINISH_CALL);
    assert_eq!(result["state"]["status"], "completed");
    assert_eq!(result["state"]["design"]["round"], 2);
    assert_eq!(
        result["state"]["tasks"]
            .as_object()
            .map(|tasks| tasks.len()),
        Some(4)
    );
    assert_eq!(
        result["state"]["tasks"]["resume-path"]["spec"]["depends_on"],
        json!(["baseline", "_design_2"])
    );

    let first_request = planner_request(&first_designer, 1);
    let second_request = planner_request(&second_designer, 2);
    assert_planner_contract(&first_request);
    assert_planner_contract(&second_request);
    let second_assignment =
        assigned_task(&second_request.body_json()).context("second designer assignment")?;
    assert!(second_assignment.contains(BASELINE_RESULT));
    assert!(second_assignment.contains("Inspect the resume-specific path"));
    assert!(second_assignment.contains("Choose which startup path to inspect"));
    let baseline_request = worker_request(&baseline, BASELINE_PROMPT, &root_thread_id);
    let followup_request = worker_request(&followup, FOLLOWUP_PROMPT, &root_thread_id);
    assert!(followup_request.body_contains_text(BASELINE_RESULT));
    for (task, request) in [
        ("_design_1", first_request),
        ("baseline", baseline_request),
        ("_design_2", second_request),
        ("resume-path", followup_request),
    ] {
        let child_id = assert_native_child(&mut app_server, &root_thread_id, &request).await?;
        assert_eq!(result["state"]["tasks"][task]["agent_id"], child_id);
        assert_eq!(result["state"]["tasks"][task]["status"], "completed");
    }
    Ok(())
}

/// An invalid design is retained for diagnosis and cannot fall back to the caller's proposal.
#[tokio::test]
async fn invalid_workflow_design_stops_before_dispatching_proposed_work() -> Result<()> {
    const WORKFLOW_ID: &str = "invalid-design";
    const WORKER_PROMPT: &str = "must-not-dispatch-proposed-work";
    let server = responses::start_mock_server().await;
    let invalid_output = design(json!([
        {"id": "proposed", "prompt": WORKER_PROMPT, "group": "work", "role": "inspect", "inputs": []}
    ]));
    let designer = planner(&server, WORKFLOW_ID, 1, invalid_output.clone()).await;
    let proposed_worker = worker(&server, WORKER_PROMPT, "unexpected fallback dispatch").await;
    let root_requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&root_requests);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(is_root_request)
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("root request JSON");
            let mut requests = captured.lock().expect("root request log");
            requests.push(body.clone());
            let response = match latest_workflow_result(&body) {
                None => workflow_call(START_CALL, json!({
                    "action": "start", "plan": {
                        "id": WORKFLOW_ID, "goal": "Inspect the evidence",
                        "tasks": [{"id": "proposed", "prompt": WORKER_PROMPT}]
                    }
                })),
                Some((_, result)) if result["state"]["status"] == "running" && requests.len() < 16 => workflow_call(
                    &format!("workflow-wait-{}", requests.len()),
                    json!({"action": "wait", "workflow_id": WORKFLOW_ID, "timeout_ms": 60000, "after_revision": result["state"]["revision"], "include_outputs": true}),
                ),
                Some((_, result)) => completed_response("root-invalid-design", &result.to_string()),
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
        "Inspect the evidence with a workflow.",
    )
    .await?;
    let requests = root_requests.lock().expect("root request log").clone();
    let (_, result) = latest_workflow_result(requests.last().context("final request")?)
        .context("invalid design result")?;
    assert_eq!(result["state"]["status"], "needs_attention");
    assert_eq!(
        result["state"]["tasks"]
            .as_object()
            .map(|tasks| tasks.len()),
        Some(1)
    );
    let failed = &result["state"]["tasks"]["_design_1"];
    assert_eq!(failed["status"], "failed");
    assert!(
        failed["error"]
            .as_str()
            .is_some_and(|error| error.contains("deliverable"))
    );
    assert_eq!(failed["output"], invalid_output.to_string());
    assert!(worker_requests(&proposed_worker, WORKER_PROMPT, &root_thread_id).is_empty());
    let request = planner_request(&designer, 1);
    assert_planner_contract(&request);
    assert_native_child(&mut app_server, &root_thread_id, &request).await?;
    Ok(())
}

/// A coordinator cannot erase a designed consumer before seeing its upstream evidence.
#[tokio::test]
async fn workflow_rejects_premature_consumer_removal_and_runs_the_evidence_handoff() -> Result<()> {
    const WORKFLOW_ID: &str = "preserve-consumer";
    const EVIDENCE_PROMPT: &str = "collect-current-tracking-observation";
    const EVIDENCE_RESULT: &str = "The retained conclusion says stable; the current recording measures three degrees of drift per minute.";
    const CONSUMER_PROMPT: &str = "cross-check-the-stability-claim-against-the-recording";
    const CONSUMER_RESULT: &str = "Cross-check: the retained stability claim conflicts with the measured drift in the current recording.";
    const REVISE_CALL: &str = "workflow-remove-consumer";

    // Keep the upstream response pending until the coordinator has received the
    // revise result. This exercises the real guard without a timing-dependent race.
    let release_evidence = Arc::new(tokio::sync::Notify::new());
    let root_requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let consumer_requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let gate = Arc::clone(&release_evidence);
    let captured_root = Arc::clone(&root_requests);
    let captured_consumer = Arc::clone(&consumer_requests);
    let router = axum::Router::new().route(
        "/v1/responses",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let gate = Arc::clone(&gate);
            let captured_root = Arc::clone(&captured_root);
            let captured_consumer = Arc::clone(&captured_consumer);
            async move {
                let response = if assigned_task(&body).is_some_and(|text| text.contains("You are executing task '_design_1'")) {
                    completed_response("protected-design", &design(json!([
                        {"id": "observation", "prompt": EVIDENCE_PROMPT, "group": "evidence", "role": "collect current observation", "deliverable": "Current measured drift and retained conclusion", "inputs": []},
                        {"id": "cross-check", "prompt": CONSUMER_PROMPT, "group": "reconciliation", "role": "check the claim against the recording", "deliverable": "Evidence-backed contradiction check", "inputs": [{"task_id": "observation", "use_for": "Compare the measured drift with the retained stability claim"}]}
                    ])).to_string())
                } else if assigned_prompt(&body, EVIDENCE_PROMPT) {
                    gate.notified().await;
                    completed_response("protected-evidence", EVIDENCE_RESULT)
                } else if assigned_prompt(&body, CONSUMER_PROMPT) {
                    captured_consumer.lock().expect("consumer request log").push(body.clone());
                    completed_response("protected-cross-check", CONSUMER_RESULT)
                } else {
                    let mut requests = captured_root.lock().expect("root request log");
                    requests.push(body.clone());
                    let removal_observed = requests.iter().any(|request| {
                        latest_workflow_result(request).is_some_and(|(call_id, _)| call_id == REVISE_CALL)
                    });
                    match latest_workflow_result(&body) {
                        None => workflow_call(START_CALL, json!({
                            "action": "start", "workflow_id": WORKFLOW_ID,
                            "goal": "Check whether the retained stability claim agrees with current tracking evidence"
                        })),
                        Some((call_id, _)) if call_id == REVISE_CALL => {
                            gate.notify_one();
                            workflow_call(STATUS_CALL, json!({"action": "status", "workflow_id": WORKFLOW_ID, "include_outputs": true}))
                        }
                        Some((_, result)) if !removal_observed
                            && result["state"]["tasks"]["observation"]["status"] == "running"
                            && result["state"]["tasks"]["cross-check"]["status"] == "pending" => workflow_call(
                                REVISE_CALL,
                                json!({
                                    "action": "revise", "workflow_id": WORKFLOW_ID,
                                    "remove_task_ids": ["cross-check"],
                                    "assessment": "Drop the reconciliation because final synthesis is already my responsibility."
                                }),
                            ),
                        Some((_, result)) if result["state"]["status"] == "awaiting_decision" => workflow_call(
                            FINISH_CALL,
                            json!({"action": "finish", "workflow_id": WORKFLOW_ID, "include_outputs": true, "assessment": "The completed cross-check identifies the contradiction between the retained claim and current recording."}),
                        ),
                        Some((_, result)) if result["state"]["status"] == "running" && requests.len() < 24 => workflow_call(
                            &format!("workflow-wait-{}", requests.len()),
                            json!({"action": "wait", "workflow_id": WORKFLOW_ID, "timeout_ms": 60000, "after_revision": result["state"]["revision"], "include_outputs": true}),
                        ),
                        Some((_, result)) => completed_response("root-protected-consumer", &result.to_string()),
                    }
                };
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], response)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let responses_server = tokio::spawn(async move { axum::serve(listener, router).await });
    let (_codex_home, mut app_server) = fixture_at_url(&url).await?;
    let root_thread_id = start_thread(&mut app_server).await?;
    run_turn(
        &mut app_server,
        &root_thread_id,
        "Check the retained tracking claim against the current evidence.",
    )
    .await?;

    let requests = root_requests.lock().expect("root request log").clone();
    let rejected = requests
        .iter()
        .find_map(|request| {
            latest_workflow_result(request)
                .filter(|(call_id, _)| call_id == REVISE_CALL)
                .map(|(_, result)| result)
        })
        .context("premature removal result")?;
    assert!(
        rejected["tool_error"].as_str().is_some_and(|error| {
            error.contains("consumes work that has not returned")
                && error.contains(
                    "Final synthesis by the coordinator does not replace this planned work",
                )
        }),
        "expected removal guard, got {rejected}"
    );
    let (call_id, result) = latest_workflow_result(requests.last().context("final root request")?)
        .context("accepted workflow")?;
    assert_eq!(call_id, FINISH_CALL);
    assert_eq!(result["state"]["status"], "completed");
    assert_eq!(
        result["state"]["tasks"]["observation"]["output"],
        EVIDENCE_RESULT
    );
    assert_eq!(
        result["state"]["tasks"]["cross-check"]["status"],
        "completed"
    );
    assert_eq!(
        result["state"]["tasks"]["cross-check"]["output"],
        CONSUMER_RESULT
    );
    assert_eq!(
        result["state"]["tasks"]["cross-check"]["spec"]["inputs"][0]["task_id"],
        "observation"
    );
    let consumers = consumer_requests.lock().expect("consumer request log");
    assert_eq!(
        consumers.len(),
        1,
        "protected consumer must execute exactly once"
    );
    assert!(assigned_task(&consumers[0]).is_some_and(|text| text.contains(EVIDENCE_RESULT)));
    responses_server.abort();
    Ok(())
}
