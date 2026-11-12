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

// `collaboration` is reserved by the API for its known native agent functions.
// Custom workflow controls must keep their own namespace, regardless of the
// configured namespace for those agent functions.
pub(crate) const WORKFLOW_TOOL_NAMESPACE: &str = "workflows";

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
            ("deliverable".into(), text("Concrete output required from this task. Required when revising work in a designed workflow.")),
            ("inputs".into(), JsonSchema::array(JsonSchema::object(BTreeMap::from([
                ("task_id".into(), text("A task in depends_on whose returned output this task consumes.")),
                ("use_for".into(), text("How that specific output determines this task's work or decision.")),
            ]), Some(vec!["task_id".into(), "use_for".into()]), Some(false.into())), Some("State the use of each work dependency; labels alone do not create handoffs.".into()))),
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
        description: "Design and execute native workflows from a complex objective. Start with workflow_id and goal (include scope and constraints); a native planning agent constructs a typed design of concrete deliverables and their information dependencies before any work is dispatched. An optional plan is only a proposal to that planner. The scheduler runs independent tasks concurrently and supplies actual predecessor reports to downstream work. When the graph returns, status becomes awaiting_decision, not completed: inspect reports against design.completion_criteria and deferred_decisions, then advance with assessment to plan remaining work from the findings, or finish with assessment when the objective is handled. Use revise for precise pending-task edits, retry for failed/interrupted work, pause/resume/cancel for execution control. Do not impose a fixed review ritual or finish merely because workers returned. Native permissions, model inheritance and shared capacity apply. Designs, revisions, decisions and reports are retained on disk. Status without workflow_id lists saved workflows.".into(),
        strict: false, defer_loading: None, output_schema: None,
        parameters: JsonSchema::object(BTreeMap::from([
            ("action".into(), JsonSchema::string_enum(["start","status","wait","advance","finish","revise","retry","pause","resume","cancel"].into_iter().map(|s| json!(s)).collect(), None)),
            ("workflow_id".into(), text("Unique short ID for start; existing workflow ID for other actions. A proposal may supply plan.id.")),
            ("plan".into(), plan),
            ("tasks".into(), tasks),
            ("remove_task_ids".into(), ids("For revise: obsolete unstarted tasks to remove. A designed consumer cannot be removed or disconnected while its work inputs are unfinished; it runs when they return. An actual scope change requires updated goal and assessment identifying the change. Update dependent edges together. Prior revisions remain.")),
            ("goal".into(), text("For start: the user's concrete objective, relevant context, scope and constraints. The planner designs the workflow; do not pre-flatten it into topic assignments. For revise: update the objective when user direction changes.")),
            ("assessment".into(), text("Required for advance or finish, and for scope-changing revisions that remove a consumer of unfinished work. Assess actual findings against completion criteria. For advance identify useful remaining work; for finish explain why the requested outcome is handled. For a scope revision identify the actual changed objective or constraint; it is retained in the journal.")),
            ("max_parallel".into(), number("For start or revise: simultaneous workers; defaults to 3 and respects shared native capacity.")),
            ("task_ids".into(), ids("Task IDs to retry. Downstream results are invalidated; running affected tasks must first be paused.")),
            ("expected_revision".into(), number("Optional revision precondition for a mutation. Omit when progress since the last status is acceptable.")),
            ("after_revision".into(), number("For wait: return on a later revision or a non-running state. Default is the revision at entry.")),
            ("timeout_ms".into(), number("Optional wait duration in milliseconds. Uses the configured native multi-agent wait defaults and limits; progress, messages, user steering, or interruption end the wait early.")),
            ("include_outputs".into(), boolean("Return complete task reports in status. Default false gives bounded excerpts and on-disk record path.")),
        ]), Some(vec!["action".into()]), Some(false.into())),
    })
}
