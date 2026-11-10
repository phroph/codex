//! Model-facing native workflow control. Execution and durable state live in workflow/.

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::multi_agents_common::function_arguments;
use crate::tools::handlers::multi_agents_common::tool_output_code_mode_result;
use crate::tools::handlers::multi_agents_common::tool_output_json_text;
use crate::tools::handlers::multi_agents_common::tool_output_response_item;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::models::ResponseInputItem;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) struct WorkflowHandler;

impl WorkflowHandler {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl ToolExecutor<ToolInvocation> for WorkflowHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("workflow")
    }

    fn spec(&self) -> ToolSpec {
        create_workflow_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let args = parse_arguments(&function_arguments(invocation.payload)?)?;
            let result = crate::workflow::execute(
                invocation.session,
                invocation.step_context,
                &invocation.call_id,
                args,
                invocation.cancellation_token,
            )
            .await
            .map_err(FunctionCallError::RespondToModel)?;
            Ok(boxed_tool_output(WorkflowOutput(result)))
        })
    }
}

impl CoreToolRuntime for WorkflowHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

struct WorkflowOutput(Value);

impl ToolOutput for WorkflowOutput {
    fn log_output(&self) -> String {
        tool_output_json_text(&self.0, "workflow")
    }
    fn success_for_logging(&self) -> bool {
        true
    }
    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, &self.0, Some(true), "workflow")
    }
    fn code_mode_result(&self, _payload: &ToolPayload) -> Value {
        tool_output_code_mode_result(&self.0, "workflow")
    }
}

pub(crate) fn create_workflow_tool() -> ToolSpec {
    let text = |description: &str| JsonSchema::string(Some(description.into()));
    let boolean = |description: &str| JsonSchema::boolean(Some(description.into()));
    let number = |description: &str| JsonSchema::number(Some(description.into()));
    let ids = |description: &str| JsonSchema::array(text("Task ID"), Some(description.into()));
    let task = JsonSchema::object(
        BTreeMap::from([
            (
                "id".into(),
                text("Unique short portable task ID: letters, digits, underscore or hyphen."),
            ),
            (
                "prompt".into(),
                text(
                    "Concrete work, scope, expected result and relevant constraints. Supply necessary context; dependency reports are added automatically.",
                ),
            ),
            (
                "group".into(),
                text("Optional workstream/group label. Does not create an extra manager agent."),
            ),
            (
                "role".into(),
                text(
                    "Optional freeform purpose, such as investigate, implement, verify a specific claim, or critique a design. No fixed role sequence is required.",
                ),
            ),
            (
                "depends_on".into(),
                ids(
                    "Predecessor task IDs whose results this task needs. Omit for independent work.",
                ),
            ),
            (
                "run_after_failure".into(),
                boolean(
                    "Allow this task after all dependencies stop, including failures. Useful for diagnosis or recovery. Default false.",
                ),
            ),
            (
                "inherit_context".into(),
                boolean(
                    "Use the coordinator's full conversation context instead of a fresh task context. Default false. Omit model/effort overrides for inherited context.",
                ),
            ),
            (
                "model".into(),
                text(
                    "Optional native child model override; preserve the current model unless user or applicable instructions request otherwise.",
                ),
            ),
            (
                "reasoning_effort".into(),
                JsonSchema::string_enum(
                    vec![
                        json!("low"),
                        json!("medium"),
                        json!("high"),
                        json!("xhigh"),
                        json!("max"),
                        json!("ultra"),
                    ],
                    Some("Optional native reasoning effort override.".into()),
                ),
            ),
        ]),
        Some(vec!["id".into(), "prompt".into()]),
        Some(false.into()),
    );
    let tasks = JsonSchema::array(
        task,
        Some(
            "Tasks forming an acyclic dependency graph. Give parallel writers separate ownership."
                .into(),
        ),
    );
    let plan = JsonSchema::object(
        BTreeMap::from([
            (
                "id".into(),
                text("Unique short workflow ID within this chat."),
            ),
            (
                "goal".into(),
                text("Overall objective and practical completion criteria that connect the tasks."),
            ),
            ("tasks".into(), tasks.clone()),
            (
                "max_parallel".into(),
                number(
                    "Maximum simultaneous workflow tasks; defaults to 3. Native shared agent limits still apply.",
                ),
            ),
        ]),
        Some(vec!["id".into(), "goal".into(), "tasks".into()]),
        Some(false.into()),
    );
    ToolSpec::Function(ResponsesApiTool {
        name: "workflow".into(),
        description: "Run and adapt native multi-agent workflows for complex work. Start a task-specific dependency graph; Codex dispatches ready tasks and passes predecessor results to successors. Tasks use native subagents and the current permissions. Use status to inspect results, wait for progress, revise to add/edit pending tasks, retry to explicitly rerun tasks and invalidate dependent results, pause to stop dispatch and interrupt workers, resume for pending work, or cancel. Interrupted tasks require explicit retry, so side effects are never silently replayed. The coordinator owns synthesis and consequential judgments; completed means a worker returned, not that its claims were verified. Choose roles and branches for the actual task; do not create mandatory review layers or new obligations from speculative caveats. Status without workflow_id lists saved workflows. All revisions/results are retained on disk.".into(),
        strict: false, defer_loading: None, output_schema: None,
        parameters: JsonSchema::object(BTreeMap::from([
            ("action".into(), JsonSchema::string_enum(["start","status","wait","revise","retry","pause","resume","cancel"].into_iter().map(|s| json!(s)).collect(), None)),
            ("workflow_id".into(), text("Workflow to inspect or manage. Start may use plan.id.")),
            ("plan".into(), plan),
            ("tasks".into(), tasks),
            ("remove_task_ids".into(), ids("For revise: obsolete unstarted tasks to remove. Update any dependent edges in the same call. Prior revisions stay in the journal.")),
            ("goal".into(), text("For revise: updated objective or completion criteria when the user's direction or findings change the plan.")),
            ("max_parallel".into(), number("For revise: update the concurrency limit for this workflow.")),
            ("task_ids".into(), ids("Task IDs to retry. Downstream results are invalidated; running affected tasks must first be paused.")),
            ("expected_revision".into(), number("Optional revision precondition for a mutation. Omit when progress since the last status is acceptable.")),
            ("after_revision".into(), number("For wait: return on a later revision or a non-running state. Default is the revision at entry.")),
            ("timeout_ms".into(), number("Optional wait duration in milliseconds. Uses the configured native multi-agent wait defaults and limits; progress, messages, user steering, or interruption end the wait early.")),
            ("include_outputs".into(), boolean("Return complete task reports in status. Default false gives bounded excerpts and on-disk record path.")),
        ]), Some(vec!["action".into()]), Some(false.into())),
    })
}
