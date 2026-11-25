//! Runtime-owned acceptance review of a designed workflow's answer.
//!
//! The coordinator that ran the investigation is a poor judge of its own
//! integrated result. Before a designed workflow completes, fresh reviewers that
//! did not author the work check the draft answer from distinct lenses. The
//! runtime builds their prompts from the user's own messages, the design and the
//! returned reports, so the coordinator cannot narrow what is checked.
//!
//! Findings are advisory, aggregated per issue by severity rather than by vote.
//! Blocking and major findings require a recorded coordinator response; they do
//! not by themselves prevent completion. The answer actually delivered must be
//! the one reviewed: if the coordinator's final answer differs from the reviewed
//! text, a delta review checks the changes before the workflow completes.

use super::model;
use super::model::ReviewIssue;
use super::model::ReviewKind;
use super::model::ReviewRecord;
use super::model::ReviewSeverity;
use super::model::RunStatus;
use super::model::TaskKind;
use super::model::TaskSpec;
use super::model::TaskStatus;
use super::model::WorkflowState;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;

/// Optional configured agent role applied to reviewers, e.g. to run them at a
/// higher reasoning effort or on another model than the workflow's workers.
pub(crate) const REVIEWER_ROLE: &str = "workflow_reviewer";
pub(super) const TASK_PREFIX: &str = "_review_";
/// Budget for the user's own messages. Long annotation-heavy threads keep the
/// first message (often the original request) plus the most recent ones.
const USER_MESSAGE_BUDGET_CHARS: usize = 60_000;
const MAX_USER_MESSAGE_CHARS: usize = 8_000;
const MAX_STANDING_INSTRUCTION_CHARS: usize = 12_000;
const REPORT_BUDGET_CHARS: usize = 48_000;
const MAX_REPORT_CHARS: usize = 12_000;
const MAX_DRAFT_CHARS: usize = 60_000;
const MAX_ASSESSMENT_CHARS: usize = 16_000;
const MAX_RESPONSE_CHARS: usize = 16_000;
const MAX_GOAL_CHARS: usize = 8_000;
const MAX_STRATEGY_CHARS: usize = 4_000;
const MAX_CRITERIA: usize = 20;
const MAX_CRITERION_CHARS: usize = 1_000;
/// Cross-lens findings are one problem when their significant words overlap
/// this much (|A∩B|/min), or a little less when their anchors also overlap.
const MERGE_WORD_OVERLAP: f64 = 0.45;
const MERGE_WORD_OVERLAP_WITH_ANCHORS: f64 = 0.35;
const MERGE_ANCHOR_OVERLAP: f64 = 0.6;
/// A changed final answer gets at most this many delta reviews per round.
const MAX_DELTA_REVIEWS: usize = 3;
/// Messages kept whole at the end of a long thread; older ones are shortened
/// before any are dropped, so short directives survive behind long pastes.
const KEEP_RECENT_MESSAGES: usize = 3;
const SHORTENED_MESSAGE_CHARS: usize = 1_200;
const MAX_CHANGE_SUMMARY_CHARS: usize = 30_000;
const OMITTED_NOTE: &str = "[earlier user messages omitted: ";
const COMPACTED_NOTE: &str =
    "[earlier conversation was compacted; its oldest user messages may be missing from this list]";
/// Marker of injected AGENTS.md instructions (`UserInstructions::type_markers`).
const AGENTS_MD_MARKER: &str = "# AGENTS.md instructions";
/// The coordinator's recent user-visible messages given to reviewers, so they
/// can see what was already told to and tried with the user.
const MAX_COORDINATOR_MESSAGES: usize = 12;
const MAX_COORDINATOR_MESSAGE_CHARS: usize = 1_500;
/// The native tool a coordinator uses to message the user while work continues.
const SEND_TO_USER_TOOL: &str = "send_message_to_user_async";
/// Values listed for a delta reviewer as dropped from the reviewed text.
const MAX_DROPPED_VALUES: usize = 60;
/// A reviewed sentence counts as dropped when fewer than this share of its
/// content words remain anywhere in the final answer.
const DROPPED_SENTENCE_OVERLAP: f64 = 0.5;
const MAX_DROPPED_SENTENCES: usize = 15;
const MAX_DROPPED_SENTENCE_CHARS: usize = 300;

pub(super) struct Lens {
    pub key: &'static str,
    pub title: &'static str,
    pub instructions: &'static str,
}

/// Distinct lenses catch failure modes a single generic reviewer misses. In an
/// offline replay of 11 real workflows the first three lenses roughly doubled
/// recall of user-caught defects over one generic reviewer with the same
/// context. Field use then showed most remaining user corrections were about
/// direction rather than claims, which the intent lens targets.
pub(super) const LENSES: [Lens; 4] = [
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
- Identify the decisive discriminator the answer itself names as missing. Check whether every available way to obtain it was tried (other vantage points, other devices on the path, other logs) before it was declared unavailable or handed back to the user. When a blocker is handed to the user, name any other device, log or path the agent could have used itself to get past it.
- List alternative explanations the evidence does not exclude, including any the user raised earlier, any the collected evidence itself hints at, and common causes in the components on the path. For each, say whether the answer excluded it with evidence.
- Check for positive and negative controls: would the query, probe or test have found the thing if it were there?"#,
    },
    Lens {
        key: "coverage",
        title: "Requirement coverage and faithful reporting",
        instructions: r#"- Enumerate every ask this workflow's request covers, read literally and in light of the symptoms the user described, the earlier conversation and the standing instructions; include an earlier ask only if it is clearly still open. Map each ask to where the draft answer handles it. Flag any ask that was narrowed, replaced with an easier question, or dropped.
- Compare the draft answer with the evidence the workflow surfaced in its reports. Flag findings that were found but omitted, downplayed or contradicted, and qualifiers dropped when numbers were restated.
- Flag completion claims ("fixed", "corrected", "restores", "ready", "verified") whose described verification does not actually cover the claim.
- Flag places where the user would have to ask again for something they already asked for."#,
    },
    Lens {
        key: "intent",
        title: "User intent, direction and actionability",
        instructions: r#"- List the user's standing directives from anywhere in their messages and standing instructions: the higher-level goal the work serves, priorities, scope limits, quality bars, and things to avoid (for example "do not lose the higher-level thread", "do not over-focus on X", "no ceremony"). A directive stays binding until the user retracts it, however long ago it was stated. Check the draft and any recommended next step against each, and quote the directive a finding conflicts with.
- Check that the answer measures and reports the quantity the user actually named (their symptom, metric or deliverable), not a nearby proxy that was easier to obtain.
- Check that the answer is actionable for the user: a clear decision or concrete next steps. A scope the coordinator set for itself (for example "diagnosis only") does not limit what the user needs; only the user's own words do. "Wait for it to happen again" or "monitor and report back" is not actionable on its own: ask what the user can do or decide today, and what the answer means concretely for them (bug, failing hardware, configuration, or something else).
- Test every recommended next step for feasibility against the conversation, including what the coordinator has already told the user: has the user said it is blocked, failing, already tried or already done? Was the same step already pushed and not worked? Could the agent get past the blocker itself through another device, log or path instead of handing it back? A step that repeats a failed or blocked path, or hands back a blocker the agent could work around, is a major finding.
- Check whether the work, and the next work it recommends, still serves the user's goal or drifts toward what is convenient, narrow, toy-scale or merely interesting, and whether it proposes process or ceremony the user has rejected.
- Leave number re-derivation, evidence sufficiency and ask-by-ask coverage to the other lenses; report direction problems."#,
    },
];

/// One reviewer applying every lens, for the compact review A/B.
const COMBINED_LENS: Lens = Lens {
    key: "combined",
    title: "All acceptance checks: numbers, sufficiency, coverage and intent",
    instructions: "Apply each of the following checklists in turn. Where a checklist leaves something to another lens, that lens is yours too.",
};

const DELTA_LENS: Lens = Lens {
    key: "delta",
    title: "Changes since the reviewed draft",
    instructions: r#"- Compare the FINAL ANSWER with the previously reviewed text. For each correction the coordinator's response claims, check that it is actually present in the final answer.
- Verify every claim, number, result or recommendation that is new in the final answer against primary evidence, as strictly as the original review would have. New experiments or results added after review are unreviewed until you check them. Facts that came from an earlier reviewer (counterexamples, counts, pairings) are claims too, not established facts.
- Agreement reached with the same method, query or matching rule as the coordinator or an earlier reviewer is not independent verification. Re-derive with a different method, or with the matching tolerance or window stated (events logged a millisecond apart are a pair).
- Flag content the user needs that was in the reviewed text but was dropped or weakened (for example observed time windows, lists of effects, base rates, caveats, next steps). Check each item of the "Specific values dropped" list above, when there is one.
- Flag direction problems the changes introduce against the user's standing directives.
- Do not re-raise findings that the response and the final answer already handle adequately."#,
};

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

/// Whether a native agent thread is an acceptance reviewer in this workflow.
pub(super) fn is_reviewer_agent(state: &WorkflowState, agent_id: &str) -> bool {
    state.tasks.values().any(|task| {
        task.spec.kind == TaskKind::Review && task.agent_id.as_deref() == Some(agent_id)
    })
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

/// The text a completed, reviewed workflow committed to deliver: the final
/// answer of the review the coordinator answered when it finished.
pub(super) fn delivered_text(state: &WorkflowState) -> Option<&str> {
    if state.status != RunStatus::Completed {
        return None;
    }
    let design = state.design.as_ref()?;
    design
        .reviews
        .iter()
        .rev()
        .find(|review| !review.superseded && review.response.is_some())
        .and_then(|review| review.final_answer.as_deref())
        .filter(|text| !text.trim().is_empty())
}

/// Whether `message` contains `text`, ignoring whitespace differences.
pub(super) fn contains_normalized(message: &str, text: &str) -> bool {
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    squash(message).contains(&squash(text))
}

/// Whether `message` carries `text`: verbatim, or with most of its content words,
/// so a reformatted answer or one combined with other text still counts.
pub(super) fn carries(message: &str, text: &str) -> bool {
    if contains_normalized(message, text) {
        return true;
    }
    let words = merge_tokens(text).into_iter().collect::<BTreeSet<_>>();
    if words.is_empty() {
        return true;
    }
    let present = merge_tokens(message).into_iter().collect::<BTreeSet<_>>();
    let kept = words.iter().filter(|word| present.contains(*word)).count();
    kept as f64 >= words.len() as f64 * DROPPED_SENTENCE_OVERLAP
}

/// A reviewer's output stays out of the coordinator's view until its review is
/// complete, so the coordinator does not act on part of the panel. Field use
/// showed coordinators posting conclusions from three of four lenses.
/// A superseded review or a cancelled run never completes its review, so its
/// returned reviewers are shown: their findings no longer gate anything.
pub(super) fn withheld(state: &WorkflowState, task_id: &str) -> bool {
    state.status != RunStatus::Cancelled
        && is_review_task(task_id)
        && state.design.as_ref().is_some_and(|design| {
            design.reviews.iter().any(|review| {
                !review.complete
                    && !review.superseded
                    && review.task_ids.iter().any(|task| task == task_id)
            })
        })
}

pub(super) const WITHHELD_OUTPUT: &str = "Withheld until the whole review panel returns; the merged findings then appear in state.design.reviews.";

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

/// Only a revision that changes the work or the objective changes what would be
/// delivered. Adjusting concurrency alone must not discard a review.
pub(super) fn revision_supersedes(tasks_changed: bool, removals: bool, goal_changed: bool) -> bool {
    tasks_changed || removals || goal_changed
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

fn reviewer_spec(attempt: u32, lens: &Lens, deliverable: &str) -> TaskSpec {
    TaskSpec {
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
        deliverable: Some(deliverable.into()),
        inputs: Vec::new(),
    }
}

fn next_attempt(state: &WorkflowState) -> Result<u32, String> {
    state
        .design
        .as_ref()
        .map_or(0, |design| design.reviews.len())
        .checked_add(1)
        .and_then(|attempt| u32::try_from(attempt).ok())
        .ok_or_else(|| "review count overflow".to_string())
}

/// Add reviewer tasks and their record, then run them.
fn queue_review(
    state: &mut WorkflowState,
    specs: Vec<TaskSpec>,
    record: ReviewRecord,
) -> Result<Vec<TaskSpec>, String> {
    let used = state
        .plan
        .tasks
        .iter()
        .filter(|task| task.kind == TaskKind::Review)
        .count();
    if used + specs.len() > model::MAX_REVIEW_TASKS {
        return Err(format!(
            "This workflow has used its allowance of {} acceptance-reviewer tasks. Deliver the last reviewed answer unchanged if one exists, or start a new workflow for further work.",
            model::MAX_REVIEW_TASKS
        ));
    }
    let mut candidate = state.clone();
    model::revise_tasks_with_removals(&mut candidate, specs.clone(), &[])?;
    candidate
        .design
        .as_mut()
        .ok_or("Missing workflow design state")?
        .reviews
        .push(ReviewRecord {
            task_ids: specs.iter().map(|spec| spec.id.clone()).collect(),
            ..record
        });
    candidate.status = RunStatus::Running;
    candidate.last_error = None;
    *state = candidate;
    Ok(specs)
}

/// Queue lens reviewers for the coordinator's draft answer. The workflow keeps
/// running until they return, then returns to awaiting_decision with findings.
/// `compact` runs one reviewer applying every lens instead of the panel.
pub(super) fn begin(
    state: &mut WorkflowState,
    assessment: &str,
    draft_answer: &str,
    context: ReviewContext,
    compact: bool,
) -> Result<Vec<TaskSpec>, String> {
    if in_progress(state) {
        return Err(stalled_message(state).unwrap_or_else(|| {
            if state.status == RunStatus::Running {
                "Acceptance reviewers are still checking the answer; wait for their findings (call wait or end your turn), then finish".into()
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
    let attempt = next_attempt(state)?;
    let deliverable = "Structured acceptance findings with severity and checked evidence";
    let specs = if compact {
        vec![reviewer_spec(attempt, &COMBINED_LENS, deliverable)]
    } else {
        LENSES
            .iter()
            .map(|lens| reviewer_spec(attempt, lens, deliverable))
            .collect()
    };
    queue_review(
        state,
        specs,
        ReviewRecord {
            round,
            attempt,
            assessment: assessment.into(),
            draft_answer: draft_answer.into(),
            user_messages: context.user_messages,
            standing_instructions: context.standing_instructions,
            coordinator_messages: context.coordinator_messages,
            ..Default::default()
        },
    )
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
    let mut issues = merge_duplicates(issues);
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

fn merge_tokens(text: &str) -> Vec<String> {
    // The same list the merge thresholds were calibrated with.
    const STOP: &[&str] = &[
        "the", "and", "for", "that", "this", "with", "are", "was", "not", "but", "from", "its",
        "has", "have", "into", "than", "then", "they", "their", "which", "while", "would",
        "should", "answer", "draft", "final", "user", "users", "does", "did", "only", "also",
        "same", "each", "both", "more", "most", "such", "when", "what", "where", "there", "here",
        "been", "being", "because", "about", "after", "before", "over", "under", "between",
        "across", "these", "those", "them", "some", "any", "all", "can", "could", "may", "might",
        "must", "will", "just", "very",
    ];
    let joiner = |c: char| matches!(c, '.' | '_' | '/' | '-');
    text.split(|c: char| !c.is_alphanumeric() && !joiner(c))
        .map(|token| token.trim_matches(joiner).to_lowercase())
        .filter(|token| token.chars().count() >= 3 && !STOP.contains(&token.as_str()))
        .collect()
}

/// Words a finding uses, and its anchors: numbers, identifiers and paths, which
/// two lenses reporting the same problem tend to share even when paraphrasing.
struct Fingerprint {
    words: BTreeSet<String>,
    anchors: BTreeSet<String>,
}

fn fingerprint(issue: &ReviewIssue) -> Fingerprint {
    let words = merge_tokens(&format!("{} {}", issue.title, issue.detail))
        .into_iter()
        .collect();
    let anchors = merge_tokens(&format!(
        "{} {} {}",
        issue.title, issue.detail, issue.evidence
    ))
    .into_iter()
    .filter(|token| {
        token
            .chars()
            .any(|c| c.is_ascii_digit() || matches!(c, '.' | '_' | '/'))
    })
    .collect();
    Fingerprint { words, anchors }
}

/// |A ∩ B| / min(|A|, |B|): paraphrases of one problem score high even when one
/// lens writes much more than the other.
fn overlap(left: &BTreeSet<String>, right: &BTreeSet<String>) -> f64 {
    let smaller = left.len().min(right.len());
    if smaller == 0 {
        return 0.0;
    }
    left.intersection(right).count() as f64 / smaller as f64
}

/// Calibrated on 155 real findings from 31 reviewed workflows: this merges the
/// cross-lens restatements found there (about one in nine findings) while
/// leaving distinct findings that share topic words apart.
fn same_problem(left: &Fingerprint, right: &Fingerprint) -> bool {
    let words = overlap(&left.words, &right.words);
    words >= MERGE_WORD_OVERLAP
        || (words >= MERGE_WORD_OVERLAP_WITH_ANCHORS
            && left.anchors.len().min(right.anchors.len()) >= 2
            && overlap(&left.anchors, &right.anchors) >= MERGE_ANCHOR_OVERLAP)
}

/// One problem found by several lenses is one issue. Merging keeps every lens's
/// title, detail and evidence, labelled by lens, so nothing a reviewer wrote is
/// lost; it stops one problem being counted, and answered, once per lens. A
/// finding is compared with each member of a group (not their union, which
/// would chain unrelated findings together), and findings from the same lens
/// are never merged with each other.
pub(super) fn merge_duplicates(issues: Vec<ReviewIssue>) -> Vec<ReviewIssue> {
    struct Group {
        issue: ReviewIssue,
        lenses: Vec<String>,
        members: Vec<Fingerprint>,
    }
    let label = |issue: &ReviewIssue| {
        (
            format!("[{} lens] {}: {}", issue.lens, issue.title, issue.detail),
            format!("[{} lens] {}", issue.lens, issue.evidence),
        )
    };
    let mut groups: Vec<Group> = Vec::new();
    for issue in issues {
        let print = fingerprint(&issue);
        let group = groups.iter_mut().find(|group| {
            !group.lenses.contains(&issue.lens)
                && group
                    .members
                    .iter()
                    .any(|member| same_problem(member, &print))
        });
        let Some(group) = group else {
            groups.push(Group {
                lenses: vec![issue.lens.clone()],
                members: vec![print],
                issue,
            });
            continue;
        };
        if group.lenses.len() == 1 {
            // First merge: label the original contributor before adding another.
            let (detail, evidence) = label(&group.issue);
            group.issue.detail = detail;
            group.issue.evidence = evidence;
        }
        let (detail, evidence) = label(&issue);
        group.issue.detail.push_str(&format!("\n\n{detail}"));
        group.issue.evidence.push_str(&format!("\n{evidence}"));
        if issue.severity < group.issue.severity {
            group.issue.severity = issue.severity;
            group.issue.title = issue.title;
        }
        group.lenses.push(issue.lens);
        group.issue.lens = group.lenses.join("+");
        group.members.push(print);
    }
    groups.into_iter().map(|group| group.issue).collect()
}

fn needs_response(review: &ReviewRecord) -> bool {
    !review.failed_reviewers.is_empty()
        || review
            .issues
            .iter()
            .any(|issue| issue.severity != ReviewSeverity::Minor)
}

fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Answer a returned review. If the final answer is the text that was reviewed,
/// the workflow completes. Otherwise a delta reviewer checks what changed, and
/// the returned specs must be dispatched; the workflow completes only once the
/// delivered text is text that was reviewed.
pub(super) fn respond(
    state: &mut WorkflowState,
    assessment: &str,
    response: Option<&str>,
    final_answer: Option<&str>,
    review_enabled: bool,
    user_context: Option<ReviewContext>,
) -> Result<Option<Vec<TaskSpec>>, String> {
    // Answering a review never resumes a paused, cancelled or stopped workflow.
    if state.status != RunStatus::AwaitingDecision {
        return Err(format!(
            "finish requires awaiting_decision; the workflow is {:?}. Resume it before answering the review.",
            state.status
        ));
    }
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
    let final_answer = final_answer.map(str::trim).filter(|text| !text.is_empty()).ok_or(
        "final_answer is required: the exact answer you will deliver to the user. If it differs from the reviewed text, a delta review checks the changes before the workflow completes.",
    )?;
    nonempty(final_answer, "final_answer")?;
    nonempty(assessment, "assessment against the completion criteria")?;
    // With review turned off, the answer is recorded and the workflow completes.
    let unchanged = !review_enabled || normalized(final_answer) == normalized(&review.draft_answer);
    let deltas_this_round = state.design.as_ref().map_or(0, |design| {
        design
            .reviews
            .iter()
            .rev()
            .take_while(|record| record.kind == ReviewKind::Delta && record.round == review.round)
            .count()
    });
    if !unchanged && deltas_this_round >= MAX_DELTA_REVIEWS {
        return Err(format!(
            "The final answer has changed after {deltas_this_round} delta reviews. Deliver the last reviewed text unchanged (finish with final_answer equal to it), or advance if the answer needs more work."
        ));
    }
    let previous = review.clone();
    let mut candidate = state.clone();
    {
        let review = active_mut(&mut candidate).ok_or("review disappeared")?;
        review.response = Some(
            response
                .unwrap_or("No blocking or major findings required a response.")
                .to_string(),
        );
        review.final_answer = Some(final_answer.to_string());
    }
    if unchanged {
        super::planning::finish(&mut candidate, assessment)?;
        *state = candidate;
        return Ok(None);
    }
    // The delivered text changed after review: check the changes, with the
    // user's messages as they are now (they may have steered since the panel).
    let context = user_context.unwrap_or(ReviewContext {
        user_messages: previous.user_messages,
        standing_instructions: previous.standing_instructions,
        coordinator_messages: previous.coordinator_messages,
    });
    let attempt = next_attempt(&candidate)?;
    let specs = queue_review(
        &mut candidate,
        vec![reviewer_spec(
            attempt,
            &DELTA_LENS,
            "Structured findings on the changes between the reviewed text and the final answer",
        )],
        ReviewRecord {
            round: previous.round,
            attempt,
            assessment: assessment.into(),
            draft_answer: final_answer.into(),
            user_messages: context.user_messages,
            standing_instructions: context.standing_instructions,
            coordinator_messages: context.coordinator_messages,
            kind: ReviewKind::Delta,
            ..Default::default()
        },
    )?;
    *state = candidate;
    Ok(Some(specs))
}

pub(super) fn summary(review: &ReviewRecord) -> String {
    let count = |severity| {
        review
            .issues
            .iter()
            .filter(|issue| issue.severity == severity)
            .count()
    };
    let what = match review.kind {
        ReviewKind::Full => "Acceptance review",
        ReviewKind::Delta => "Delta review of the final answer",
    };
    let mut text = format!(
        "{what} {} returned {} blocking, {} major and {} minor findings",
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
                "{}. The workflow is {:?}; resume it before answering the findings and finishing.",
                summary(review),
                state.status
            ));
        }
        return Some(format!(
            "{}. Read the latest entry of state.design.reviews. For each blocking or major finding, correct the answer, disprove it with evidence, advance to do further work, or accept it as a stated risk. Facts a reviewer introduces (counterexamples, counts, pairings, measurements) are claims: re-derive each one fully, not only its magnitude, before putting it in the answer. Then call finish with assessment, review_response and final_answer: the exact text you will deliver. If final_answer differs from the reviewed text, a short delta review checks the changes first. Deliver final_answer unchanged once the workflow completes.",
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
        return Some("Acceptance reviewers are checking the answer. Do not poll, sleep or message the reviewers: call wait with this workflow_id, or end your turn; the runtime continues you when their findings return.".into());
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
pub(super) fn user_messages<'a>(items: impl Iterator<Item = &'a ResponseItem>) -> ReviewContext {
    let mut messages = Vec::new();
    let mut standing = None;
    let mut compacted = false;
    let mut coordinator = Vec::new();
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
        // What the coordinator already told the user: answers, async messages
        // sent while work continued, and progress commentary.
        if let Some((told, priority)) = coordinator_message(item) {
            coordinator.push((
                priority,
                format!(
                    "[after user message {}] {}",
                    messages.len(),
                    clip(told.trim(), MAX_COORDINATOR_MESSAGE_CHARS)
                ),
            ));
            continue;
        }
        let Some(TurnItem::UserMessage(message)) = crate::event_mapping::parse_turn_item(item)
        else {
            continue;
        };
        let text = message.message();
        if crate::compact::is_summary_message(&text) {
            compacted = true;
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
    let mut kept = fit_messages(messages);
    if compacted {
        // The live window no longer holds the oldest messages; say so rather
        // than presenting a surviving message as the original request.
        kept.insert(0, COMPACTED_NOTE.to_string());
    }
    ReviewContext {
        user_messages: kept,
        standing_instructions: standing,
        coordinator_messages: recent_coordinator_messages(coordinator),
    }
}

/// The most recent answers and async messages first, then the most recent
/// commentary, in their original order. A long run of progress preambles must
/// not push out the recommendation the user already rejected.
fn recent_coordinator_messages(entries: Vec<(bool, String)>) -> Vec<String> {
    let mut keep = vec![false; entries.len()];
    let mut remaining = MAX_COORDINATOR_MESSAGES;
    for wanted in [true, false] {
        for (index, (priority, _)) in entries.iter().enumerate().rev() {
            if remaining == 0 {
                break;
            }
            if *priority == wanted && !keep[index] {
                keep[index] = true;
                remaining -= 1;
            }
        }
    }
    entries
        .into_iter()
        .zip(keep)
        .filter_map(|((_, text), keep)| keep.then_some(text))
        .collect()
}

/// The user's own words and what the coordinator already told them, captured
/// from the owner conversation when a review starts.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ReviewContext {
    pub user_messages: Vec<String>,
    pub standing_instructions: Option<String>,
    pub coordinator_messages: Vec<String>,
}

/// User-visible text from the coordinator, and whether it is an answer or an
/// async message (true) rather than progress commentary (false). Agent mail
/// stored with the assistant role is not something the user was told.
fn coordinator_message(item: &ResponseItem) -> Option<(String, bool)> {
    let (text, priority) = match item {
        ResponseItem::Message {
            role,
            content,
            phase,
            ..
        } if role == "assistant" => {
            if InterAgentCommunication::is_message_content(content) {
                return None;
            }
            let text = content
                .iter()
                .filter_map(|part| match part {
                    ContentItem::OutputText { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (text, !matches!(phase, Some(MessagePhase::Commentary)))
        }
        ResponseItem::FunctionCall {
            name, arguments, ..
        } if name == SEND_TO_USER_TOOL => (
            serde_json::from_str::<Value>(arguments)
                .ok()?
                .get("message")?
                .as_str()?
                .to_string(),
            true,
        ),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some((text, priority))
}

/// How many messages an omission note stands for, if this entry is one.
fn omitted_count(entry: &str) -> Option<usize> {
    entry
        .strip_prefix(OMITTED_NOTE)?
        .split(';')
        .next()?
        .trim()
        .parse()
        .ok()
}

fn is_note(entry: &str) -> bool {
    entry == COMPACTED_NOTE || omitted_count(entry).is_some()
}

/// Keep every message when the thread fits the budget. Otherwise keep the first
/// message (often the original request) and the most recent ones whole, shorten
/// the rest so short directives survive behind long pastes, and only then drop
/// the oldest shortened messages, noting how many.
fn fit_messages(messages: Vec<String>) -> Vec<String> {
    let total = |list: &[String]| list.iter().map(|text| text.chars().count()).sum::<usize>();
    if total(&messages) <= USER_MESSAGE_BUDGET_CHARS || messages.len() < 2 {
        return messages;
    }
    let count = messages.len();
    let mut kept = messages
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            if index == 0 || index + KEEP_RECENT_MESSAGES >= count {
                text
            } else {
                clip(&text, SHORTENED_MESSAGE_CHARS)
            }
        })
        .collect::<Vec<_>>();
    let mut omitted = 0;
    while total(&kept) > USER_MESSAGE_BUDGET_CHARS && kept.len() > 1 + KEEP_RECENT_MESSAGES {
        kept.remove(1);
        omitted += 1;
    }
    if omitted > 0 {
        kept.insert(
            1,
            format!("{OMITTED_NOTE}{omitted}; the full conversation is in the owner thread]"),
        );
    }
    kept
}

/// Whether a word-like token names something specific: an identifier, file or
/// path, rather than prose such as "e.g." or "left/right".
fn is_identifier(token: &str) -> bool {
    if !token.chars().any(char::is_alphabetic) || token.chars().count() < 3 {
        return false;
    }
    if token.contains('_') || token.contains('\\') {
        return true;
    }
    if token.contains('/') {
        return token.starts_with('/') || token.contains('.');
    }
    // Dotted names and files ("quest_log.txt", "com.oculus.vrshell"), not
    // abbreviations made of single letters ("e.g", "i.e").
    token.contains('.')
        && token
            .split('.')
            .filter(|part| part.chars().count() >= 2)
            .count()
            >= 2
}

/// Numbers with their sign, percent, ratio or a short unit word ("-3.2", "5%",
/// "4/249", "3 days", "35 ms"), and identifiers or paths, in their original case.
fn value_tokens(text: &str) -> Vec<String> {
    let inner = |c: char| matches!(c, '.' | '_' | '/' | '-' | ':' | '%' | '\\');
    // Words with their byte spans, so a unit is joined only across one space.
    let mut words: Vec<(usize, usize)> = Vec::new();
    let mut start = None;
    for (index, c) in text.char_indices() {
        if c.is_alphanumeric() || inner(c) {
            start.get_or_insert(index);
        } else if let Some(begin) = start.take() {
            words.push((begin, index));
        }
    }
    if let Some(begin) = start {
        words.push((begin, text.len()));
    }
    let mut tokens = Vec::new();
    for (index, &(begin, end)) in words.iter().enumerate() {
        let word = &text[begin..end];
        let ends_sentence = word.ends_with(['.', ':']);
        // A leading '/' marks an absolute path, so it is kept.
        let mut token = word
            .trim_start_matches(|c: char| matches!(c, '.' | '_' | ':' | '\\'))
            .trim_end_matches(|c: char| matches!(c, '.' | '_' | '/' | ':' | '\\'));
        // A leading '-' is kept only as the sign of a number.
        if token.starts_with('-') && !token[1..].starts_with(|c: char| c.is_ascii_digit()) {
            token = token.trim_start_matches('-');
        }
        token = token.trim_end_matches('-');
        if token.is_empty() {
            continue;
        }
        if token.chars().any(|c| c.is_ascii_digit()) {
            let numeric = token
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '/' | ':' | ','));
            let unit = words
                .get(index + 1)
                .filter(|(next, _)| *next == end + 1 && &text[end..*next] == " ")
                // A unit may end the sentence ("every 3 days.").
                .map(|&(next, next_end)| {
                    text[next..next_end].trim_end_matches(|c: char| !c.is_alphabetic())
                })
                .filter(|unit| {
                    (1..=5).contains(&unit.chars().count()) && unit.chars().all(char::is_alphabetic)
                });
            // Small counts mean little without their unit ("3 days", "35 ms");
            // longer numbers are specific on their own.
            match unit {
                Some(unit) if numeric && !ends_sentence && token.chars().count() <= 2 => {
                    tokens.push(format!("{token} {unit}"))
                }
                _ => tokens.push(token.to_string()),
            }
        } else if is_identifier(token) {
            tokens.push(token.to_string());
        }
    }
    tokens
}

/// Sentences of the reviewed text whose content words mostly do not appear in
/// the final answer: dropped caveats and statements a token list cannot see.
pub(super) fn dropped_sentences(before: &str, after: &str) -> Vec<String> {
    let present = merge_tokens(after).into_iter().collect::<BTreeSet<_>>();
    let mut dropped = Vec::new();
    let mut sentence = String::new();
    let mut chars = before.chars().peekable();
    while let Some(c) = chars.next() {
        sentence.push(c);
        let mut end = c == '\n';
        if matches!(c, '.' | '!' | '?') {
            // Closing markup or quotes after the stop belong to the sentence
            // ("**Both builds matched.** Next ..." ends at the bold marker).
            while let Some(&next) = chars.peek()
                && matches!(next, '*' | '_' | '`' | ')' | ']' | '"' | '\'' | '”' | '’')
            {
                sentence.push(next);
                chars.next();
            }
            end = chars.peek().is_none_or(|next| next.is_whitespace());
        }
        if !end && chars.peek().is_some() {
            continue;
        }
        let words = merge_tokens(&sentence).into_iter().collect::<BTreeSet<_>>();
        if words.len() >= 4 {
            let kept = words.iter().filter(|word| present.contains(*word)).count();
            if (kept as f64) < words.len() as f64 * DROPPED_SENTENCE_OVERLAP {
                dropped.push(clip(sentence.trim(), MAX_DROPPED_SENTENCE_CHARS));
            }
        }
        sentence.clear();
        if dropped.len() >= MAX_DROPPED_SENTENCES {
            break;
        }
    }
    dropped
}

fn quoted_spans(text: &str) -> Vec<String> {
    let mut spans = Vec::new();
    for (open, close) in [('“', '”'), ('"', '"'), ('`', '`')] {
        // An unpaired straight quote (an inch mark, say) would invert every
        // later pair, so straight quotes are used only when they pair up.
        if open == close && text.matches(open).count() % 2 != 0 {
            continue;
        }
        let mut rest = text;
        while let Some(start) = rest.find(open) {
            let after = &rest[start + open.len_utf8()..];
            let Some(end) = after.find(close) else {
                break;
            };
            let span = after[..end].trim();
            if (3..=80).contains(&span.chars().count()) {
                spans.push(span.to_string());
            }
            rest = &after[end + close.len_utf8()..];
        }
    }
    spans
}

/// Specific values in the reviewed text (numbers with their context, identifiers,
/// paths and quoted spans) that no longer appear in the final answer. When every
/// paragraph is rewritten a paragraph diff says nothing, but a dropped base rate
/// or build number still shows up here.
pub(super) fn dropped_values(before: &str, after: &str) -> Vec<String> {
    let after_lower = after.to_lowercase();
    // A number with a unit word also counts as its bare number, so "step 5" is
    // still present when the final answer says "5 retries".
    let after_tokens = value_tokens(after)
        .into_iter()
        .flat_map(|token| {
            let token = token.to_lowercase();
            let number = token.split(' ').next().unwrap_or_default().to_string();
            [token, number]
        })
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    let mut dropped = Vec::new();
    let values = value_tokens(before)
        .into_iter()
        .filter(|token| {
            let token = token.to_lowercase();
            // A specific number ("1/310", "58.3", "35") survives a changed
            // unit word; a single digit needs its unit to match.
            let number = token.split(' ').next().unwrap_or_default();
            !after_tokens.contains(&token)
                && !(number.chars().count() > 1 && after_tokens.contains(number))
        })
        .chain(
            quoted_spans(before)
                .into_iter()
                .filter(|span| !after_lower.contains(&span.to_lowercase())),
        );
    for value in values {
        if seen.insert(value.to_lowercase()) {
            dropped.push(value);
        }
        if dropped.len() >= MAX_DROPPED_VALUES {
            break;
        }
    }
    dropped
}

/// Paragraphs removed from and added to a text, for a delta reviewer to check
/// directly rather than infer from two clipped copies.
fn changed_paragraphs(before: &str, after: &str) -> String {
    let paragraphs = |text: &str| {
        text.replace("\r\n", "\n")
            .split("\n\n")
            .map(|paragraph| paragraph.trim().to_string())
            .filter(|paragraph| !paragraph.is_empty())
            .collect::<Vec<_>>()
    };
    let (before, after) = (paragraphs(before), paragraphs(after));
    let key = |paragraph: &String| normalized(paragraph);
    let before_keys = before.iter().map(key).collect::<BTreeSet<_>>();
    let after_keys = after.iter().map(key).collect::<BTreeSet<_>>();
    let mut changes = String::new();
    for paragraph in before
        .iter()
        .filter(|paragraph| !after_keys.contains(&key(paragraph)))
    {
        changes.push_str(&format!("\n[REMOVED]\n{paragraph}\n"));
    }
    for paragraph in after
        .iter()
        .filter(|paragraph| !before_keys.contains(&key(paragraph)))
    {
        changes.push_str(&format!("\n[ADDED OR REWRITTEN]\n{paragraph}\n"));
    }
    if changes.is_empty() {
        return "(Only formatting or paragraph order changed.)\n".into();
    }
    clip(&changes, MAX_CHANGE_SUMMARY_CHARS)
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
    let Some(design) = state.design.as_ref() else {
        return format!(
            "Acceptance review task {id} has no workflow design; report that as a blocking finding."
        );
    };
    let position = design
        .reviews
        .iter()
        .position(|review| review.task_ids.iter().any(|task| task == id));
    let lens = LENSES
        .iter()
        .chain([&DELTA_LENS, &COMBINED_LENS])
        .find(|lens| id.ends_with(&format!("_{}", lens.key)));
    let (Some(position), Some(lens)) = (position, lens) else {
        return format!(
            "Acceptance review task {id} has no matching review record; report that as a blocking finding."
        );
    };
    let review = &design.reviews[position];
    let delta = review.kind == ReviewKind::Delta;
    let combined = lens.key == COMBINED_LENS.key;
    let mut prompt = if delta {
        format!(
            "You are an independent acceptance reviewer for native workflow '{}'. The coordinator's answer was already reviewed by a panel, then changed. You check the FINAL ANSWER the coordinator intends to deliver against the previously reviewed text and the panel's findings. You did not produce this work.\n\n",
            state.plan.id
        )
    } else if combined {
        format!(
            "You are the independent acceptance reviewer for native workflow '{}'. The coordinator produced the DRAFT ANSWER below for the user's request and intends to deliver it. You did not produce this work. You are the only reviewer, so apply every checklist in your lens below. Find what is wrong or missing before the user acts on it.\n\n",
            state.plan.id
        )
    } else {
        format!(
            "You are one specialist on an independent acceptance review panel for native workflow '{}'. The coordinator produced the DRAFT ANSWER below for the user's request and intends to deliver it. You did not produce this work. Find what is wrong or missing before the user acts on it, strictly within your lens; other specialists cover other angles.\n\n",
            state.plan.id
        )
    };
    prompt.push_str("Default to skepticism. A claim is supported only if you checked it against primary evidence (raw logs, captures, data files, meshes/CAD, current source, or the output of a test or script you ran) or it follows directly from evidence quoted here. Agreement with a worker report or the coordinator's assessment is not verification. Report concrete, specific, checkable issues ranked by consequence. Generic caveats count as noise and false alarms count against you. If within your lens the answer is sound, return verdict accept with no issues or only minor ones.\n\nA fact you introduce (a counterexample, count, pairing, measurement or match) is a new claim the coordinator may deliver to the user. State in its evidence exactly how you derived it: the file, query or script, and the matching tolerance or time window (for example, whether events logged milliseconds apart count as a pair). If you could not derive it rigorously, say so instead of stating it as fact.\n\nYou are read-only: inspect files and run read-only commands. Run analysis inline (for example python -c, or a script piped on stdin) rather than writing files. Do not modify project files, settings, devices or services, and do not dispatch agents. Messages that arrive while you work are not part of your assignment.\n\nSeverity: blocking = acting on the answer as written would mislead or harm the user (a load-bearing claim is wrong, or a requested deliverable is missing or broken). major = a load-bearing claim is unsupported, an ask was narrowed or dropped, the evidence cannot carry the conclusion, or the answer conflicts with a standing user directive. minor = worth noting; it does not change what the user should do.\n");
    prompt.push_str("\n## The user's messages (verbatim, oldest first)\nThis workflow serves the request near the end of this list, including any later steering. Specific asks in earlier messages may already have been answered in earlier turns; flag one as dropped only if it is clearly still open or this workflow's request depends on it. Directives about goals, priorities, scope, quality or what to avoid stay in force wherever they appear unless the user retracted them.\n");
    if review.user_messages.is_empty() {
        prompt.push_str(
            "(Not available from the retained conversation; rely on the workflow objective.)\n",
        );
    }
    // Notes are not user messages: they are not numbered, and the numbers of the
    // messages after an omission note keep their real position in the thread.
    let total = review
        .user_messages
        .iter()
        .map(|entry| match omitted_count(entry) {
            Some(count) => count,
            None if is_note(entry) => 0,
            None => 1,
        })
        .sum::<usize>();
    let mut number = 0;
    for entry in &review.user_messages {
        if is_note(entry) {
            number += omitted_count(entry).unwrap_or(0);
            prompt.push_str(&format!("\n{entry}\n"));
            continue;
        }
        number += 1;
        let label = if number == total {
            " (most recent)"
        } else {
            ""
        };
        prompt.push_str(&format!("\n### User message {number}{label}\n{entry}\n"));
    }
    if let Some(standing) = &review.standing_instructions {
        prompt.push_str(&format!("\n## Standing user instructions\n{standing}\n"));
    }
    if !review.coordinator_messages.is_empty() {
        prompt.push_str("\n## What the user has already been told (coordinator messages, oldest first)\nEach entry is tagged with the user message it followed. Use these to see what the user already knows, what was already tried or recommended, and how the user reacted. They are the coordinator's claims, not evidence.\n");
        for entry in &review.coordinator_messages {
            prompt.push_str(&format!("\n- {entry}\n"));
        }
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
    if delta {
        // The panel's review of this round and every delta answered since, so a
        // later delta still sees the panel's draft, findings and responses.
        let base = (0..position).rev().find(|index| {
            design.reviews[*index].kind == ReviewKind::Full
                && design.reviews[*index].round == review.round
        });
        if let Some(base) = base {
            prompt.push_str(&format!(
                "\n## Draft reviewed by the panel\n{}\n",
                clip(&design.reviews[base].draft_answer, MAX_DRAFT_CHARS)
            ));
            for earlier in &design.reviews[base..position] {
                prompt.push_str(&format!("\n## {}\n", summary(earlier)));
                if !earlier.failed_reviewers.is_empty() {
                    prompt.push_str("Some reviewers did not return a usable review; treat what they were to check as unreviewed.\n");
                }
                for issue in &earlier.issues {
                    // The evidence carries how a reviewer derived its facts, so a
                    // delta reviewer can check them with a different method.
                    prompt.push_str(&format!(
                        "- [{:?}/{}] {}: {}\n  Evidence and method: {}\n",
                        issue.severity,
                        issue.lens,
                        issue.title,
                        clip(&issue.detail, 4_000),
                        clip(&issue.evidence, 2_000)
                    ));
                }
                if let Some(response) = &earlier.response {
                    prompt.push_str(&format!(
                        "\nCoordinator's response:\n{}\n",
                        clip(response, MAX_RESPONSE_CHARS)
                    ));
                }
            }
        }
        if let Some(previous) = position
            .checked_sub(1)
            .and_then(|index| design.reviews.get(index))
        {
            if base != Some(position - 1) {
                prompt.push_str(&format!(
                    "\n## Text before this change (last reviewed by a delta review)\n{}\n",
                    clip(&previous.draft_answer, MAX_DRAFT_CHARS)
                ));
            }
            prompt.push_str(&format!(
                "\n## Paragraphs changed since that text\n{}",
                changed_paragraphs(&previous.draft_answer, &review.draft_answer)
            ));
            // Against the panel's draft too: a failed earlier delta must not
            // hide what the first change dropped.
            let mut sources = vec![previous.draft_answer.as_str()];
            if let Some(base) = base
                && base != position - 1
            {
                sources.insert(0, design.reviews[base].draft_answer.as_str());
            }
            let mut values = Vec::new();
            let mut sentences = Vec::new();
            for source in sources {
                for value in dropped_values(source, &review.draft_answer) {
                    if !values.contains(&value) && values.len() < MAX_DROPPED_VALUES {
                        values.push(value);
                    }
                }
                for sentence in dropped_sentences(source, &review.draft_answer) {
                    if !sentences.contains(&sentence) && sentences.len() < MAX_DROPPED_SENTENCES {
                        sentences.push(sentence);
                    }
                }
            }
            if !values.is_empty() {
                prompt.push_str("\n## Specific values dropped since that text\nThese numbers, identifiers and quoted spans appear in the reviewed text but not in the final answer. For each, decide whether dropping it is correct (it was wrong, superseded or immaterial) or loses something the user needs.\n");
                for value in values {
                    prompt.push_str(&format!("- {value}\n"));
                }
            }
            if !sentences.is_empty() {
                prompt.push_str("\n## Reviewed sentences largely absent from the final answer\nMost of the content words of these sentences appear nowhere in the final answer. Check each for a dropped caveat, statement or next step the user needs.\n");
                for sentence in sentences {
                    prompt.push_str(&format!("- {sentence}\n"));
                }
            }
        }
        prompt.push_str(&format!(
            "\n## Coordinator's assessment\n{}\n\n## FINAL ANSWER UNDER REVIEW (to be delivered)\n{}\n",
            clip(&review.assessment, MAX_ASSESSMENT_CHARS),
            clip(&review.draft_answer, MAX_DRAFT_CHARS)
        ));
    } else {
        prompt.push_str(&format!(
            "\n## Coordinator's assessment\n{}\n\n## DRAFT ANSWER UNDER REVIEW\n{}\n",
            clip(&review.assessment, MAX_ASSESSMENT_CHARS),
            clip(&review.draft_answer, MAX_DRAFT_CHARS)
        ));
    }
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
        "\n## Your lens: {}\n{}\n",
        lens.title, lens.instructions
    ));
    if combined {
        for part in &LENSES {
            prompt.push_str(&format!("\n### {}\n{}\n", part.title, part.instructions));
        }
    }
    prompt.push_str(
        "\nReturn the structured review. List in checks_run what you actually inspected or ran.\n",
    );
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
