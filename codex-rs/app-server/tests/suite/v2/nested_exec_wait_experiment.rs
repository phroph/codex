//! Foreground process completion, counted at the mock Responses transport.
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use test_case::test_case;
use wiremock::Mock;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[test_case(json!({}), 0, 2; "coalesces_progress_by_default")]
#[test_case(json!({}), 7, 2; "failure_delivered_once")]
#[test_case(json!({"yield_time_ms":1000}), 0, 2; "foreground_absorbs_short_yield")]
#[test_case(json!({"yield_time_ms":10000}), 0, 2; "foreground_absorbs_explicit_default")]
#[test_case(json!({"yield_time_ms":1000,"background":true}), 0, 3; "background_preserves_short_yield")]
#[test_case(json!({"background":true}), 0, 3; "background_preserves_implicit_yield")]
#[test_case(json!({"yield_time_ms":1000,"timeout_ms":60000}), 0, 3; "foreground_preserves_explicit_timeout")]
#[test_case(json!({"yield_time_ms":1000,"tty":true}), 0, 3; "foreground_preserves_tty")]
#[test_case(json!({"yield_time_ms":1000}), 7, 2; "foreground_failure_delivered_once")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_exec_wait_experiment(
    extra_args: Value,
    exit_code: i32,
    expected_requests: usize,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let count = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&count);
    let command = if cfg!(windows) {
        format!(
            "Write-Output progress-1; Start-Sleep -Seconds 4; Write-Output progress-2; Start-Sleep -Seconds 4; Write-Output progress-3; Start-Sleep -Seconds 4; Write-Output completion-marker; exit {exit_code}"
        )
    } else {
        format!(
            "printf 'progress-1\\n'; sleep 4; printf 'progress-2\\n'; sleep 4; printf 'progress-3\\n'; sleep 4; printf 'completion-marker\\n'; exit {exit_code}"
        )
    };
    let mut args = json!({"cmd": command, "login": false});
    args.as_object_mut()
        .unwrap()
        .extend(extra_args.as_object().unwrap().clone());
    let code = format!("text(await tools.exec_command({args}));");
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let index = observed.fetch_add(1, Ordering::SeqCst);
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let output = body["input"].as_array().unwrap().iter().rev().find_map(|item| {
                (item["type"] == "custom_tool_call_output")
                    .then(|| item["output"].as_array().unwrap())
                    .and_then(|items| items.get(1))
                    .and_then(|item| item["text"].as_str())
                    .and_then(|text| serde_json::from_str::<Value>(text).ok())
            });
            let item = if index == 0 {
                responses::ev_custom_tool_call("start", "exec", &code)
            } else if let Some(process_id) = output.as_ref().and_then(|v| v["session_id"].as_i64()) {
                assert!(index < 4, "fixture exceeded its bounded polling allowance");
                responses::ev_custom_tool_call(
                    "poll",
                    "exec",
                    &format!("text(await tools.write_stdin({{session_id:{process_id},chars:'',yield_time_ms:10000}}));"),
                )
            } else {
                let output = output.expect("final command output must be JSON");
                assert_eq!(output["exit_code"], json!(exit_code));
                assert!(output["output"].as_str().unwrap().contains("completion-marker"));
                responses::ev_assistant_message("done", "done")
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses::sse(vec![
                    responses::ev_response_created(&format!("response-{index}")),
                    item,
                    responses::ev_completed(&format!("response-{index}")),
                ]))
        })
        .mount(&server)
        .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::CodeModeOnly)
        // This fixture runs fixed output/sleep commands in the test harness.
        .with_sandbox_mode("danger-full-access")
        .with_root_config("model_reasoning_effort = \"ultra\"\nservice_tier = \"default\"")
        .with_provider_config("supports_websockets = false")
        .with_extra_config("[features.code_mode]\ndefault_exec_yield_time_ms = 120000\n[features.multi_agent_v2]\nenabled = true\nmin_wait_timeout_ms = 1500000\ndefault_wait_timeout_ms = 1500000\nmax_wait_timeout_ms = 1500000")
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .with_env_overrides(&[
            ("CODEX_EXPERIMENT_NESTED_EXEC_WAIT", None),
            ("CODEX_EXPERIMENT_FOREGROUND_EXEC", None),
        ])
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?;
    let id = app
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.thread.id,
            input: vec![UserInput::Text {
                text: "Run the fixture and wait for its result.".into(),
                text_elements: vec![],
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = app.read_response(id).await?;
    let done: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(done.turn.status, TurnStatus::Completed);
    assert_eq!(count.load(Ordering::SeqCst), expected_requests);
    Ok(())
}
