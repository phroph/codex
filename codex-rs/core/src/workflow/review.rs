//! Runtime-owned acceptance review of a designed workflow's draft answer.
//!
//! The coordinator that ran the investigation is a poor judge of its own
//! integrated result. Before a designed workflow completes, fresh reviewers that
//! did not author the work check the draft answer from distinct lenses. The
//! runtime builds their prompts from the user's own messages, the design and the
//! returned reports, so the coordinator cannot narrow what is checked.
//!
//! Findings are advisory, aggregated per issue by severity rather than by vote.
//! Blocking and major findings require a recorded coordinator response; they do
//! not by themselves prevent completion.

use super::model;
use super::model::ReviewIssue;
use super::model::ReviewRecord;
use super::model::ReviewSeverity;
use super::model::RunStatus;
use super::model::TaskKind;
use super::model::TaskSpec;
use super::model::TaskStatus;
use super::model::WorkflowState;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

/// Optional configured agent role applied to reviewers, e.g. to run them at a
/// higher reasoning effort or on another model than the workflow's workers.
pub(crate) const REVIEWER_ROLE: &str = "workflow_reviewer";
pub(super) const TASK_PREFIX: &str = "_review_";
const MAX_USER_MESSAGES: usize = 12;
const MAX_USER_MESSAGE_CHARS: usize = 8_000;
const MAX_STANDING_INSTRUCTION_CHARS: usize = 12_000;
const REPORT_BUDGET_CHARS: usize = 48_000;
const MAX_REPORT_CHARS: usize = 12_000;
const MAX_DRAFT_CHARS: usize = 60_000;
const MAX_ASSESSMENT_CHARS: usize = 16_000;
const MAX_GOAL_CHARS: usize = 8_000;
const MAX_STRATEGY_CHARS: usize = 4_000;
const MAX_CRITERIA: usize = 20;
const MAX_CRITERION_CHARS: usize = 1_000;
/// Marker of injected AGENTS.md instructions (`UserInstructions::type_markers`).
const AGENTS_MD_MARKER: &str = "# AGENTS.md instructions";

pub(super) struct Lens {
    pub key: &'static str,
    pub title: &'static str,
    pub instructions: &'static str,
}

/// Distinct lenses catch failure modes a single generic reviewer misses. In an
/// offline replay of 11 real workflows these three lenses roughly doubled recall
/// of user-caught defects over one generic reviewer with the same context.
pub(super) const LENSES: [Lens; 3] = [
    Lens {
        key: "measure",
        title: "Re-derive numbers and mechanisms from primary data",
        instructions: r#"- For every load-bearing number or mechanism the user will act on, re-derive it yourself from the primary artifact: raw logs, captures, data files, meshes/CAD/STL/3MF, source, or the actual output of a script or test you run. Agreement with a report, a design parameter or a README is not verification; a parameter is an intention, the artifact is the fact.
- Watch for quantities measured along the wrong axis or under the wrong definition: vertical versus normal thickness, nominal versus as-built, peak versus sustained, per-face versus total.
- For physical or manufactured deliverables, measure the actual geometry against process capability: minimum wall and feature thickness measured normal to the surface (for FDM, structural walls of about two nozzle widths, never below one), text and label glyph height and stroke width against the nozzle, fits and clearances against typical print error (about ±0.1–0.2 mm) including tight channels, snap/catch/latch engagement depth (overlap below about 0.5 mm is fragile), retention under combined rotation and translation rather than axis-aligned moves only, and bridges versus cantilevers. Compute these with an inline script (for example python -c or a script piped on stdin) rather than estimating them; report what you ran.
- For systems, networks and software, check that the claimed mechanism is the one the primary data shows, rather than an alternative the same data supports equally well."#,
    },
    Lens {
        key: "sufficiency",
        title: "Evidence sufficiency and stopping rules",
        instructions: r#"- When the conclusion depends on observing an intermittent event, compare the observation window with the event's base rate and compute how likely the window was to catch it. Flag any monitoring, capture or test that was stopped on a clean window when only a positive observation could settle the question, and say what should have been left running.
- Identify the decisive discriminator the answer itself names as missing. Check whether every available way to obtain it was tried (other vantage points, other devices on the path, other logs) before it was declared unavailable or handed back to the user.
- List alternative explanations the evidence does not exclude, including any the user raised earlier, any the collected evidence itself hints at, and common causes in the components on the path. For each, say whether the answer excluded it with evidence.
- Check for positive and negative controls: would the query, probe or test have found the thing if it were there?"#,
    },
    Lens {
        key: "coverage",
        title: "Requirement coverage and faithful reporting",
        instructions: r#"- Enumerate every ask in the user's messages, read literally and in light of the symptoms they described, the earlier conversation and the standing instructions. Map each ask to where the draft answer handles it. Flag any ask that was narrowed, replaced with an easier question, or dropped.
- Compare the draft answer with the evidence the workflow surfaced in its reports. Flag findings that were found but omitted, downplayed or contradicted, and qualifiers dropped when numbers were restated.
- Flag completion claims ("fixed", "corrected", "restores", "ready", "verified") whose described verification does not actually cover the claim.
- Flag places where the user would have to ask again for something they already asked for."#,
    },
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewOutput {
    #[allow(dead_code)]
    verdict: String,
    issues: Vec<ReviewOutputIssue>,
    #[allow(dead_code)]
    checks_run: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewOutputIssue {
    severity: ReviewSeverity,
    title: String,
    detail: String,
    evidence: String,
}

pub(super) fn output_schema() -> Value {
    let text = json!({"type":"string"});
    json!({
        "type":"object", "additionalProperties":false,
        "properties":{
            "verdict":{"type":"string","enum":["accept","revise"]},
            "issues":{"type":"array","items":{
                "type":"object","additionalProperties":false,
                "properties":{
                    "severity":{"type":"string","enum":["blocking","major","minor"]},
                    "title":text,"detail":text,"evidence":text
                },
                "required":["severity","title","detail","evidence"]
            }},
            "checks_run":{"type":"array","items":text}
        },
        "required":["verdict","issues","checks_run"]
    })
}

pub(super) fn is_review_task(id: &str) -> bool {
    id.starts_with(TASK_PREFIX)
}

/// The review for the current design round that later work has not superseded.
pub(super) fn active(state: &WorkflowState) -> Option<&ReviewRecord> {
    let design = state.design.as_ref()?;
    design
        .reviews
        .last()
        .filter(|review| !review.superseded && review.round == design.round)
}

fn active_mut(state: &mut WorkflowState) -> Option<&mut ReviewRecord> {
    let design = state.design.as_mut()?;
    let round = design.round;
    design
        .reviews
        .last_mut()
        .filter(|review| !review.superseded && review.round == round)
}

/// A returned review whose findings the coordinator has not yet answered.
pub(super) fn awaiting_response(state: &WorkflowState) -> Option<&ReviewRecord> {
    active(state).filter(|review| review.complete && review.response.is_none())
}

pub(super) fn in_progress(state: &WorkflowState) -> bool {
    active(state).is_some_and(|review| !review.complete)
}

/// Mark the current review stale when further work changes what would be
/// delivered. Its unstarted reviewers are cancelled so they do not hold capacity
/// ahead of the new work; the native agent IDs of running reviewers are returned
/// for the caller to interrupt.
pub(super) fn supersede(state: &mut WorkflowState) -> Vec<String> {
    let Some(review) = active_mut(state) else {
        return Vec::new();
    };
    if review.response.is_some() {
        return Vec::new();
    }
    review.superseded = true;
    let task_ids = review.task_ids.clone();
    let mut running = Vec::new();
    for id in task_ids {
        let Some(task) = state.tasks.get_mut(&id) else {
            continue;
        };
        match task.status {
            TaskStatus::Pending | TaskStatus::Blocked => {
                task.status = TaskStatus::Cancelled;
                task.error = Some("Acceptance review superseded by later work".into());
            }
            TaskStatus::Running => running.extend(task.agent_id.clone()),
            _ => {}
        }
    }
    running
}

/// Reviewers that stopped without a result (interrupted by pause, restart or
/// cancellation). They must be retried; they are never counted as a review.
fn stalled_reviewers(state: &WorkflowState) -> Vec<String> {
    active(state)
        .filter(|review| !review.complete)
        .map(|review| {
            review
                .task_ids
                .iter()
                .filter(|id| {
                    state.tasks.get(*id).is_some_and(|task| {
                        model::is_terminal(task.status) && !reviewer_settled(task.status)
                    })
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// A reviewer settled with a result: a returned report, or a definite failure.
fn reviewer_settled(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Completed | TaskStatus::Failed)
}

/// Why an incomplete review is not progressing, for a workflow left outside Running.
pub(super) fn stalled_message(state: &WorkflowState) -> Option<String> {
    let stalled = stalled_reviewers(state);
    (!stalled.is_empty()).then(|| {
        format!(
            "Acceptance review is incomplete: reviewers {} stopped before returning findings. Retry them with action retry and these task_ids; an interrupted review is never counted as acceptance.",
            stalled.join(", ")
        )
    })
}

/// An explicit retry of only the current reviewers reopens that review; any
/// other retry invalidates it. Returns whether the review was reopened.
pub(super) fn reopen_for_retry(state: &mut WorkflowState, task_ids: &[String]) -> bool {
    let Some(review) = active_mut(state) else {
        return false;
    };
    if review.response.is_some()
        || task_ids.is_empty()
        || !task_ids.iter().all(|id| review.task_ids.contains(id))
    {
        return false;
    }
    review.complete = false;
    review.issues.clear();
    review.failed_reviewers.clear();
    true
}

/// Queue lens reviewers for the coordinator's draft answer. The workflow keeps
/// running until they return, then returns to awaiting_decision with findings.
pub(super) fn begin(
    state: &mut WorkflowState,
    assessment: &str,
    draft_answer: &str,
    user_messages: Vec<String>,
    standing_instructions: Option<String>,
) -> Result<Vec<TaskSpec>, String> {
    if in_progress(state) {
        return Err(stalled_message(state).unwrap_or_else(|| {
            if state.status == RunStatus::Running {
                "Acceptance reviewers are still checking the draft answer; wait for their findings, then finish with review_response".into()
            } else {
                "Acceptance review is incomplete and its reviewers are not running; resume the workflow to run them".into()
            }
        }));
    }
    if state.status != RunStatus::AwaitingDecision {
        return Err("finish requires awaiting_decision after all planned work has returned".into());
    }
    if state
        .tasks
        .values()
        .any(|task| !model::is_terminal(task.status))
    {
        return Err("finish cannot bypass unfinished work".into());
    }
    let round = state
        .design
        .as_ref()
        .ok_or("This legacy workflow has no design lifecycle")?
        .round;
    nonempty(assessment, "assessment against the completion criteria")?;
    nonempty(
        draft_answer,
        "draft_answer (the complete answer you intend to give the user; acceptance review checks it before the workflow completes)",
    )?;
    let attempt = state
        .design
        .as_ref()
        .map_or(0, |design| design.reviews.len())
        .checked_add(1)
        .and_then(|attempt| u32::try_from(attempt).ok())
        .ok_or("review count overflow")?;
    let specs = LENSES
        .iter()
        .map(|lens| TaskSpec {
            id: format!("{TASK_PREFIX}{attempt}_{}", lens.key),
            prompt: lens.instructions.into(),
            group: Some("Acceptance review".into()),
            role: Some(format!("Independent review: {}", lens.title)),
            depends_on: Vec::new(),
            run_after_failure: false,
            model: None,
            reasoning_effort: None,
            inherit_context: false,
            kind: TaskKind::Review,
            deliverable: Some(
                "Structured acceptance findings with severity and checked evidence".into(),
            ),
            inputs: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut candidate = state.clone();
    model::revise_tasks_with_removals(&mut candidate, specs.clone(), &[])?;
    let design = candidate
        .design
        .as_mut()
        .ok_or("Missing workflow design state")?;
    design.reviews.push(ReviewRecord {
        round,
        attempt,
        assessment: assessment.into(),
        draft_answer: draft_answer.into(),
        user_messages,
        standing_instructions,
        task_ids: specs.iter().map(|spec| spec.id.clone()).collect(),
        ..Default::default()
    });
    candidate.status = RunStatus::Running;
    candidate.last_error = None;
    *state = candidate;
    Ok(specs)
}

/// Aggregate the current review once every reviewer has returned a result or
/// definitely failed. Failures are reported as such, never counted as
/// acceptance; interrupted reviewers keep the review open until retried.
pub(super) fn collect(state: &mut WorkflowState) -> bool {
    let Some(review) = active(state) else {
        return false;
    };
    if review.complete
        || !review.task_ids.iter().all(|id| {
            state
                .tasks
                .get(id)
                .is_some_and(|task| reviewer_settled(task.status))
        })
    {
        return false;
    }
    let attempt_prefix = format!("{TASK_PREFIX}{}_", review.attempt);
    let mut issues = Vec::new();
    let mut failed = Vec::new();
    for id in &review.task_ids {
        let task = &state.tasks[id];
        let lens = id.strip_prefix(&attempt_prefix).unwrap_or(id).to_string();
        if task.status != TaskStatus::Completed {
            failed.push(format!(
                "{lens}: {:?}{}",
                task.status,
                task.error
                    .as_deref()
                    .map(|error| format!(": {error}"))
                    .unwrap_or_default()
            ));
            continue;
        }
        match serde_json::from_str::<ReviewOutput>(task.output.as_deref().unwrap_or("")) {
            Ok(output) => issues.extend(output.issues.into_iter().map(|issue| ReviewIssue {
                lens: lens.clone(),
                severity: issue.severity,
                title: issue.title,
                detail: issue.detail,
                evidence: issue.evidence,
            })),
            Err(error) => failed.push(format!("{lens}: invalid structured review: {error}")),
        }
    }
    // Stable sort keeps each lens's own ranking within a severity.
    issues.sort_by_key(|issue| issue.severity);
    let Some(review) = active_mut(state) else {
        return false;
    };
    review.issues = issues;
    review.failed_reviewers = failed;
    review.complete = true;
    true
}

fn needs_response(review: &ReviewRecord) -> bool {
    !review.failed_reviewers.is_empty()
        || review
            .issues
            .iter()
            .any(|issue| issue.severity != ReviewSeverity::Minor)
}

/// Complete a reviewed workflow. Blocking and major findings, and reviewers that
/// failed to return, require the coordinator's recorded response.
pub(super) fn respond_and_finish(
    state: &mut WorkflowState,
    assessment: &str,
    response: Option<&str>,
) -> Result<(), String> {
    let review =
        awaiting_response(state).ok_or("No returned acceptance review awaits a response")?;
    let response = response.map(str::trim).filter(|text| !text.is_empty());
    // A review in which no reviewer returned findings is not a review. Require
    // one retry of the failed reviewers before a response can stand in for it.
    let usable = review.task_ids.len() - review.failed_reviewers.len().min(review.task_ids.len());
    let retried = review
        .task_ids
        .iter()
        .all(|id| state.tasks.get(id).is_some_and(|task| task.attempt >= 2));
    if usable == 0 && !retried {
        return Err(format!(
            "{}. No reviewer returned a usable review; retry them with action retry and task_ids {} before finishing. After a repeated failure, finish with a review_response explaining it.",
            summary(review),
            review.task_ids.join(", ")
        ));
    }
    if needs_response(review) && response.is_none() {
        return Err(format!(
            "{}. Supply review_response addressing each blocking and major finding and any failed reviewer: corrected in the final answer, disproved with evidence, or accepted as a stated risk with rationale. Use advance when the findings require further work.",
            summary(review)
        ));
    }
    let response = response
        .unwrap_or("No blocking or major findings required a response.")
        .to_string();
    let mut candidate = state.clone();
    super::planning::finish(&mut candidate, assessment)?;
    if let Some(review) = active_mut(&mut candidate) {
        review.response = Some(response);
    }
    *state = candidate;
    Ok(())
}

pub(super) fn summary(review: &ReviewRecord) -> String {
    let count = |severity| {
        review
            .issues
            .iter()
            .filter(|issue| issue.severity == severity)
            .count()
    };
    let mut text = format!(
        "Acceptance review {} returned {} blocking, {} major and {} minor findings",
        review.attempt,
        count(ReviewSeverity::Blocking),
        count(ReviewSeverity::Major),
        count(ReviewSeverity::Minor)
    );
    if !review.failed_reviewers.is_empty() {
        text.push_str(&format!(
            "; {} reviewer(s) did not return a usable review ({})",
            review.failed_reviewers.len(),
            review.failed_reviewers.join("; ")
        ));
    }
    text
}

/// Coordinator guidance for the current review state, if any applies.
pub(super) fn coordinator_hint(state: &WorkflowState, review_enabled: bool) -> Option<String> {
    if state.status == RunStatus::Cancelled {
        return None;
    }
    if let Some(review) = awaiting_response(state) {
        if state.status != RunStatus::AwaitingDecision {
            return Some(format!(
                "{}. The workflow is {:?}; resume it before answering the findings and finishing with review_response.",
                summary(review),
                state.status
            ));
        }
        return Some(format!(
            "{}. Read the latest entry of state.design.reviews. For each blocking or major finding, correct the final answer, disprove it with evidence, advance to do further work, or accept it as a stated risk. Then call finish with assessment and review_response.",
            summary(review)
        ));
    }
    if in_progress(state) {
        if let Some(message) = stalled_message(state) {
            return Some(message);
        }
        if state.status != RunStatus::Running {
            return Some(format!(
                "Acceptance review is incomplete and its reviewers are not running while the workflow is {:?}; resume it to run them.",
                state.status
            ));
        }
        return Some("Acceptance reviewers are checking the draft answer; the workflow returns to awaiting_decision with their findings.".into());
    }
    (review_enabled && state.design.is_some() && state.status == RunStatus::AwaitingDecision).then(|| {
        "When the requested outcome is handled, call finish with assessment and draft_answer (the complete answer you intend to give the user). Independent acceptance reviewers check it before the workflow completes.".into()
    })
}

/// The user's own words from the owner conversation, classified with the same
/// rules the transcript uses: contextual fragments (AGENTS.md, environment,
/// skills) and media labels are not user text, and a compaction summary is
/// model-written even though it is stored with the user role. Injected AGENTS.md
/// text is kept separately as standing instructions.
pub(super) fn user_messages<'a>(
    items: impl Iterator<Item = &'a ResponseItem>,
) -> (Vec<String>, Option<String>) {
    let mut messages = Vec::new();
    let mut standing = None;
    for item in items {
        if let ResponseItem::Message { role, content, .. } = item
            && role == "user"
        {
            for part in content {
                if let ContentItem::InputText { text } = part
                    && text.trim_start().starts_with(AGENTS_MD_MARKER)
                {
                    standing = Some(clip(text.trim(), MAX_STANDING_INSTRUCTION_CHARS));
                }
            }
        }
        let Some(TurnItem::UserMessage(message)) = crate::event_mapping::parse_turn_item(item)
        else {
            continue;
        };
        let text = message.message();
        if crate::compact::is_summary_message(&text) {
            continue;
        }
        let attachments = message
            .content
            .iter()
            .filter(|input| !matches!(input, UserInput::Text { .. }))
            .count();
        let mut text = text.trim().to_string();
        if attachments > 0 {
            text.push_str(&format!(
                "\n[The user attached {attachments} non-text input(s); inspect the conversation's referenced files if they matter.]"
            ));
        }
        if !text.trim().is_empty() {
            messages.push(clip(&text, MAX_USER_MESSAGE_CHARS));
        }
    }
    let skip = messages.len().saturating_sub(MAX_USER_MESSAGES);
    (messages.into_iter().skip(skip).collect(), standing)
}

/// Keep the beginning and the end: conclusions often sit at the end of a report.
pub(super) fn clip(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    let head = limit * 2 / 3;
    let tail = limit - head;
    format!(
        "{}\n[… {} characters omitted; see the full record …]\n{}",
        text.chars().take(head).collect::<String>(),
        count - head - tail,
        text.chars().skip(count - tail).collect::<String>()
    )
}

/// Build a reviewer's assignment from runtime state rather than coordinator prose.
pub(super) fn prompt(state: &WorkflowState, id: &str, directory: &std::path::Path) -> String {
    let lens = LENSES
        .iter()
        .find(|lens| id.ends_with(&format!("_{}", lens.key)));
    let review = state.design.as_ref().and_then(|design| {
        design
            .reviews
            .iter()
            .find(|r| r.task_ids.iter().any(|t| t == id))
    });
    let (Some(lens), Some(review)) = (lens, review) else {
        return format!(
            "Acceptance review task {id} has no matching review record; report that as a blocking finding."
        );
    };
    let design = state.design.as_ref().expect("review implies design");
    let mut prompt = format!(
        "You are one specialist on an independent acceptance review panel for native workflow '{}'. The coordinator produced the DRAFT ANSWER below for the user's request and intends to deliver it. You did not produce this work. Find what is wrong or missing before the user acts on it, strictly within your lens; other specialists cover other angles.\n\nDefault to skepticism. A claim is supported only if you checked it against primary evidence (raw logs, captures, data files, meshes/CAD, current source, or the output of a test or script you ran) or it follows directly from evidence quoted here. Agreement with a worker report or the coordinator's assessment is not verification. Report concrete, specific, checkable issues ranked by consequence. Generic caveats count as noise and false alarms count against you. If within your lens the answer is sound, return verdict accept with no issues or only minor ones.\n\nYou are read-only: inspect files and run read-only commands. Run analysis inline (for example python -c, or a script piped on stdin) rather than writing files. Do not modify project files, settings, devices or services, and do not dispatch agents.\n\nSeverity: blocking = acting on the answer as written would mislead or harm the user (a load-bearing claim is wrong, or a requested deliverable is missing or broken). major = a load-bearing claim is unsupported, an ask was narrowed or dropped, or the evidence cannot carry the conclusion. minor = worth noting; it does not change what the user should do.\n",
        state.plan.id
    );
    prompt.push_str("\n## The user's messages (verbatim, oldest first)\nThis workflow serves the request near the end of this list, including any later steering. Earlier messages are context and may already have been answered in earlier turns; flag an ask from an earlier message as dropped only if it is clearly still open.\n");
    if review.user_messages.is_empty() {
        prompt.push_str(
            "(Not available from the retained conversation; rely on the workflow objective.)\n",
        );
    }
    let count = review.user_messages.len();
    for (index, message) in review.user_messages.iter().enumerate() {
        let label = if index + 1 == count {
            " (most recent)"
        } else {
            ""
        };
        prompt.push_str(&format!(
            "\n### User message {}{label}\n{message}\n",
            index + 1
        ));
    }
    if let Some(standing) = &review.standing_instructions {
        prompt.push_str(&format!("\n## Standing user instructions\n{standing}\n"));
    }
    prompt.push_str(&format!(
        "\n## Workflow objective (written by the coordinator)\n{}\n",
        clip(&state.plan.goal, MAX_GOAL_CHARS)
    ));
    if !design.completion_criteria.is_empty() {
        prompt.push_str("\n## Completion criteria (from the workflow design)\n");
        for criterion in design.completion_criteria.iter().take(MAX_CRITERIA) {
            prompt.push_str(&format!("- {}\n", clip(criterion, MAX_CRITERION_CHARS)));
        }
    }
    if !design.strategy.is_empty() {
        prompt.push_str(&format!(
            "\n## Design strategy\n{}\n",
            clip(&design.strategy, MAX_STRATEGY_CHARS)
        ));
    }
    prompt.push_str(&format!(
        "\n## Coordinator's assessment\n{}\n\n## DRAFT ANSWER UNDER REVIEW\n{}\n",
        clip(&review.assessment, MAX_ASSESSMENT_CHARS),
        clip(&review.draft_answer, MAX_DRAFT_CHARS)
    ));
    let reports = state
        .plan
        .tasks
        .iter()
        .filter(|spec| spec.kind == TaskKind::Work)
        .filter_map(|spec| state.tasks.get(&spec.id))
        .collect::<Vec<_>>();
    if !reports.is_empty() {
        prompt.push_str(&format!(
            "\n## Workflow reports (claims to check, not proof). Full records: {}\n",
            directory.display()
        ));
        let limit = (REPORT_BUDGET_CHARS / reports.len()).min(MAX_REPORT_CHARS);
        for task in reports {
            prompt.push_str(&format!(
                "\n--- Task {} ({:?}): {} ---\n{}\n",
                task.spec.id,
                task.status,
                task.spec
                    .deliverable
                    .as_deref()
                    .or(task.spec.role.as_deref())
                    .unwrap_or(""),
                clip(
                    task.output
                        .as_deref()
                        .or(task.error.as_deref())
                        .unwrap_or("No report"),
                    limit
                )
            ));
        }
    }
    prompt.push_str(&format!(
        "\n## Your lens: {}\n{}\n\nReturn the structured review. List in checks_run what you actually inspected or ran.\n",
        lens.title, lens.instructions
    ));
    prompt
}

fn nonempty(text: &str, name: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.len() > 256 * 1024 {
        return Err(format!("{name} must be nonempty and at most 256 KiB"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "review_tests.rs"]
mod tests;
