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
use codex_protocol::models::ResponseItem;
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
- Identify the decisive discriminator the answer itself names as missing. Check whether every available way to obtain it was tried (other vantage points, other devices on the path, other logs) before it was declared unavailable or handed back to the user.
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
- Check that the answer is actionable for the user: a clear decision or concrete next steps. A scope the coordinator set for itself (for example "diagnosis only") does not limit what the user needs; only the user's own words do.
- Check whether the work, and the next work it recommends, still serves the user's goal or drifts toward what is convenient, narrow, toy-scale or merely interesting, and whether it proposes process or ceremony the user has rejected.
- Leave number re-derivation, evidence sufficiency and ask-by-ask coverage to the other lenses; report direction problems."#,
    },
];

const DELTA_LENS: Lens = Lens {
    key: "delta",
    title: "Changes since the reviewed draft",
    instructions: r#"- Compare the FINAL ANSWER with the previously reviewed text. For each correction the coordinator's response claims, check that it is actually present in the final answer.
- Verify every claim, number, result or recommendation that is new in the final answer against primary evidence, as strictly as the original review would have. New experiments or results added after review are unreviewed until you check them.
- Flag content the user needs that was in the reviewed text but was dropped or weakened (for example observed time windows, lists of effects, caveats, next steps).
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
    let specs = LENSES
        .iter()
        .map(|lens| {
            reviewer_spec(
                attempt,
                lens,
                "Structured acceptance findings with severity and checked evidence",
            )
        })
        .collect();
    queue_review(
        state,
        specs,
        ReviewRecord {
            round,
            attempt,
            assessment: assessment.into(),
            draft_answer: draft_answer.into(),
            user_messages,
            standing_instructions,
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
    user_context: Option<(Vec<String>, Option<String>)>,
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
    let (user_messages, standing_instructions) =
        user_context.unwrap_or((previous.user_messages, previous.standing_instructions));
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
            user_messages,
            standing_instructions,
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
            "{}. Read the latest entry of state.design.reviews. For each blocking or major finding, correct the answer, disprove it with evidence, advance to do further work, or accept it as a stated risk. Then call finish with assessment, review_response and final_answer: the exact text you will deliver. If final_answer differs from the reviewed text, a short delta review checks the changes first. Deliver final_answer unchanged once the workflow completes.",
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
pub(super) fn user_messages<'a>(
    items: impl Iterator<Item = &'a ResponseItem>,
) -> (Vec<String>, Option<String>) {
    let mut messages = Vec::new();
    let mut standing = None;
    let mut compacted = false;
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
    (kept, standing)
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
        .chain(std::iter::once(&DELTA_LENS))
        .find(|lens| id.ends_with(&format!("_{}", lens.key)));
    let (Some(position), Some(lens)) = (position, lens) else {
        return format!(
            "Acceptance review task {id} has no matching review record; report that as a blocking finding."
        );
    };
    let review = &design.reviews[position];
    let delta = review.kind == ReviewKind::Delta;
    let mut prompt = if delta {
        format!(
            "You are an independent acceptance reviewer for native workflow '{}'. The coordinator's answer was already reviewed by a panel, then changed. You check the FINAL ANSWER the coordinator intends to deliver against the previously reviewed text and the panel's findings. You did not produce this work.\n\n",
            state.plan.id
        )
    } else {
        format!(
            "You are one specialist on an independent acceptance review panel for native workflow '{}'. The coordinator produced the DRAFT ANSWER below for the user's request and intends to deliver it. You did not produce this work. Find what is wrong or missing before the user acts on it, strictly within your lens; other specialists cover other angles.\n\n",
            state.plan.id
        )
    };
    prompt.push_str("Default to skepticism. A claim is supported only if you checked it against primary evidence (raw logs, captures, data files, meshes/CAD, current source, or the output of a test or script you ran) or it follows directly from evidence quoted here. Agreement with a worker report or the coordinator's assessment is not verification. Report concrete, specific, checkable issues ranked by consequence. Generic caveats count as noise and false alarms count against you. If within your lens the answer is sound, return verdict accept with no issues or only minor ones.\n\nYou are read-only: inspect files and run read-only commands. Run analysis inline (for example python -c, or a script piped on stdin) rather than writing files. Do not modify project files, settings, devices or services, and do not dispatch agents. Messages that arrive while you work are not part of your assignment.\n\nSeverity: blocking = acting on the answer as written would mislead or harm the user (a load-bearing claim is wrong, or a requested deliverable is missing or broken). major = a load-bearing claim is unsupported, an ask was narrowed or dropped, the evidence cannot carry the conclusion, or the answer conflicts with a standing user directive. minor = worth noting; it does not change what the user should do.\n");
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
                    prompt.push_str(&format!(
                        "- [{:?}/{}] {}: {}\n",
                        issue.severity,
                        issue.lens,
                        issue.title,
                        clip(&issue.detail, 4_000)
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
