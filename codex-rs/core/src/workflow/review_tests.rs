use super::*;
use crate::workflow::planning;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;

const LENS_KEYS: [&str; 4] = ["measure", "sufficiency", "coverage", "intent"];
const DRAFT: &str = "DRAFT: the network path is ruled out; no further capture is needed.";

/// A designed workflow whose single evidence task returned, at its checkpoint.
fn returned_workflow() -> WorkflowState {
    let mut state = planning::start(
        "review-test".into(),
        "Diagnose the intermittent RDP failures".into(),
        2,
        None,
        "owner".into(),
    )
    .unwrap();
    complete(&mut state, "_design_1", "DESIGN RECEIPT: planner prose");
    let design = json!({
        "strategy":"Correlate captures with failures",
        "completion_criteria":["Identify the failing component from original evidence"],
        "deferred_decisions":[],
        "tasks":[{"id":"capture","group":"Evidence","role":"Collect captures","prompt":"Capture traffic","deliverable":"Capture correlation","inputs":[]}]
    });
    planning::apply_design(&mut state, "_design_1", &design.to_string()).unwrap();
    complete(
        &mut state,
        "capture",
        "WORKER REPORT: the 10-minute capture window was clean",
    );
    state.status = RunStatus::AwaitingDecision;
    state
}

fn complete(state: &mut WorkflowState, id: &str, output: &str) {
    let task = state.tasks.get_mut(id).unwrap();
    task.status = TaskStatus::Completed;
    task.output = Some(output.into());
}

fn begin_review(state: &mut WorkflowState) -> Vec<TaskSpec> {
    begin(
        state,
        "The capture was clean, so the path is not at fault",
        DRAFT,
        vec![
            "USER: keep capturing until a bad-MAC failure is recorded".into(),
            "Do not lose the higher level thread".into(),
        ],
        Some("# AGENTS.md instructions\nFinish the work.".into()),
    )
    .unwrap()
}

/// Answer a returned review with review enabled and no new user messages.
fn answer(
    state: &mut WorkflowState,
    assessment: &str,
    response: Option<&str>,
    final_answer: Option<&str>,
) -> Result<Option<Vec<TaskSpec>>, String> {
    respond(state, assessment, response, final_answer, true, None)
}

fn review_output(issues: Value) -> String {
    json!({"verdict":"revise","issues":issues,"checks_run":["read capture-summary.json"]})
        .to_string()
}

fn issue(severity: &str, title: &str) -> Value {
    json!({"severity":severity,"title":title,"detail":format!("{title} detail"),"evidence":"capture-summary.json"})
}

/// Return every reviewer of attempt 1 with the given issues for the first lens
/// and none for the others, then aggregate at the checkpoint.
fn returned_review(state: &mut WorkflowState, first_lens_issues: Value) {
    for (index, lens) in LENS_KEYS.iter().enumerate() {
        let issues = if index == 0 {
            first_lens_issues.clone()
        } else {
            json!([])
        };
        complete(state, &format!("_review_1_{lens}"), &review_output(issues));
    }
    assert!(collect(state));
    state.status = RunStatus::AwaitingDecision;
}

#[test]
fn acceptance_review_requires_checkpoint_assessment_and_draft_answer() {
    let mut state = returned_workflow();
    let mut running = state.clone();
    running.status = RunStatus::Running;
    assert!(begin(&mut running, "done", "answer", Vec::new(), None).is_err());
    let before = state.clone();
    let error = begin(&mut state, "done", " ", Vec::new(), None).unwrap_err();
    assert!(error.contains("draft_answer"));
    assert!(begin(&mut state, " ", "answer", Vec::new(), None).is_err());
    assert_eq!(state, before);
}

#[test]
fn acceptance_review_queues_fresh_runtime_owned_lens_reviewers() {
    let mut state = returned_workflow();
    let specs = begin_review(&mut state);
    let ids = specs
        .iter()
        .map(|spec| spec.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            "_review_1_measure",
            "_review_1_sufficiency",
            "_review_1_coverage",
            "_review_1_intent"
        ]
    );
    for spec in &specs {
        assert_eq!(spec.kind, TaskKind::Review);
        assert!(!spec.inherit_context);
        assert!(spec.depends_on.is_empty());
    }
    assert_eq!(state.status, RunStatus::Running);
    // The panel runs together even though the workflow's max_parallel is 2.
    assert_eq!(state.plan.max_parallel, 2);
    assert_eq!(model::ready_tasks(&state), ids);
    let review = active(&state).unwrap();
    assert!(!review.complete);
    assert_eq!(review.round, 1);
    assert!(in_progress(&state));
    assert!(awaiting_response(&state).is_none());
    // A second finish while reviewers run explains the wait instead of reviewing twice.
    assert_eq!(state.status, RunStatus::Running);
    let error = begin(&mut state, "done", "answer", Vec::new(), None).unwrap_err();
    assert!(error.contains("still checking"));
    // If the run stopped (for example, paused) the message says how to restart them.
    state.status = RunStatus::Paused;
    let error = begin(&mut state, "done", "answer", Vec::new(), None).unwrap_err();
    assert!(error.contains("resume"));
}

#[test]
fn acceptance_review_reviewers_do_not_take_work_capacity() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    for lens in &LENS_KEYS[..2] {
        state
            .tasks
            .get_mut(&format!("_review_1_{lens}"))
            .unwrap()
            .status = TaskStatus::Running;
    }
    // Two running reviewers leave the workflow's two work slots free.
    assert_eq!(model::running_work(&state), 0);
    let filler = (0..3)
        .map(|index| TaskSpec {
            id: format!("work-{index}"),
            prompt: "Collect evidence".into(),
            group: None,
            role: None,
            depends_on: Vec::new(),
            run_after_failure: false,
            model: None,
            reasoning_effort: None,
            inherit_context: false,
            kind: TaskKind::Work,
            deliverable: Some("Evidence".into()),
            inputs: Vec::new(),
        })
        .collect::<Vec<_>>();
    model::revise_tasks_with_removals(&mut state, filler, &[]).unwrap();
    assert_eq!(
        model::ready_tasks(&state),
        vec!["_review_1_coverage", "_review_1_intent", "work-0", "work-1"]
    );
}

#[test]
fn acceptance_review_prompt_uses_users_words_draft_and_reports_not_planner_prose() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    let prompt = super::super::task_prompt(
        &state,
        "_review_1_sufficiency",
        std::path::Path::new("records"),
    );
    assert!(prompt.contains("USER: keep capturing until a bad-MAC failure is recorded"));
    assert!(prompt.contains("Finish the work."));
    assert!(prompt.contains("DRAFT: the network path is ruled out"));
    assert!(prompt.contains("WORKER REPORT: the 10-minute capture window was clean"));
    assert!(prompt.contains("Identify the failing component from original evidence"));
    assert!(prompt.contains("Evidence sufficiency and stopping rules"));
    assert!(prompt.contains("You did not produce this work"));
    assert!(prompt.contains("read-only"));
    assert!(prompt.contains("Messages that arrive while you work are not part of your assignment"));
    assert!(!prompt.contains("DESIGN RECEIPT"));
    // Each reviewer stays within its own lens.
    assert!(!prompt.contains("Requirement coverage and faithful reporting"));
}

#[test]
fn acceptance_review_intent_lens_keeps_conversational_directives_binding() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    let prompt =
        super::super::task_prompt(&state, "_review_1_intent", std::path::Path::new("records"));
    assert!(prompt.contains("User intent, direction and actionability"));
    assert!(prompt.contains("Do not lose the higher level thread"));
    assert!(prompt.contains("stay in force wherever they appear"));
    assert!(prompt.contains("quantity the user actually named"));
    assert!(prompt.contains("only the user's own words do"));
    assert!(prompt.contains("conflicts with a standing user directive"));
    // Specific earlier asks stay gated; directives do not.
    assert!(prompt.contains("flag one as dropped only if it is clearly still open"));
}

#[test]
fn acceptance_review_long_threads_keep_short_directives_and_real_message_numbers() {
    let paste = |index: usize| format!("pasted log {index} {}", "x".repeat(7_000));
    // Long pastes around a short mid-thread directive: shortening keeps it whole.
    let mut messages = vec!["ORIGINAL REQUEST".to_string()];
    messages.extend((0..10).map(paste));
    messages.push("DIRECTIVE: do not lose the higher level thread".to_string());
    messages.extend((10..20).map(paste));
    messages.push("LATEST STEERING".to_string());
    let kept = fit_messages(messages);
    assert_eq!(kept.len(), 23, "nothing needed to be dropped");
    assert_eq!(kept[0], "ORIGINAL REQUEST");
    assert!(kept.contains(&"DIRECTIVE: do not lose the higher level thread".to_string()));
    assert!(kept[1].contains("characters omitted"));
    assert_eq!(kept.last().unwrap(), "LATEST STEERING");
    assert!(kept.iter().map(|text| text.chars().count()).sum::<usize>() <= 60_000);
    // A thread too long even when shortened drops its oldest middle messages.
    let mut messages = vec!["ORIGINAL REQUEST".to_string()];
    messages.extend((0..80).map(paste));
    messages.push("LATEST STEERING".to_string());
    let kept = fit_messages(messages);
    assert_eq!(kept[0], "ORIGINAL REQUEST");
    let omitted = omitted_count(&kept[1]).expect("omission note");
    // Kept messages plus those the note stands for account for the whole thread.
    assert_eq!(kept.len() - 1 + omitted, 82);
    assert_eq!(kept.last().unwrap(), "LATEST STEERING");
    assert!(kept.iter().map(|text| text.chars().count()).sum::<usize>() <= 60_100);
    let short = vec!["a".to_string(), "b".to_string()];
    assert_eq!(fit_messages(short.clone()), short);
    // Notes are not numbered as user messages, and numbering keeps real positions.
    let mut state = returned_workflow();
    begin(
        &mut state,
        "assessment",
        DRAFT,
        vec![
            COMPACTED_NOTE.to_string(),
            "FIRST".into(),
            format!("{OMITTED_NOTE}5; the full conversation is in the owner thread]"),
            "LAST".into(),
        ],
        None,
    )
    .unwrap();
    let prompt = super::super::task_prompt(
        &state,
        "_review_1_coverage",
        std::path::Path::new("records"),
    );
    assert!(prompt.contains("### User message 1\nFIRST"));
    assert!(prompt.contains("### User message 7 (most recent)\nLAST"));
    assert!(prompt.contains("earlier conversation was compacted"));
    assert!(!prompt.contains("### User message 2\n"));
}

#[test]
fn acceptance_review_aggregates_by_severity_and_reports_failed_reviewers() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    assert!(!collect(&mut state));
    complete(
        &mut state,
        "_review_1_measure",
        &review_output(json!([issue("minor", "Label wording")])),
    );
    complete(
        &mut state,
        "_review_1_sufficiency",
        &review_output(json!([
            issue("major", "Alternative not excluded"),
            issue("blocking", "Capture stopped on a clean window")
        ])),
    );
    complete(&mut state, "_review_1_intent", &review_output(json!([])));
    // A reviewer returning free text is a failed review, not an acceptance.
    complete(&mut state, "_review_1_coverage", "Looks fine to me.");
    assert!(collect(&mut state));
    assert!(!collect(&mut state));
    let review = awaiting_response(&state).unwrap();
    let ranked = review
        .issues
        .iter()
        .map(|issue| (issue.severity, issue.lens.as_str(), issue.title.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        ranked,
        vec![
            (
                ReviewSeverity::Blocking,
                "sufficiency",
                "Capture stopped on a clean window"
            ),
            (
                ReviewSeverity::Major,
                "sufficiency",
                "Alternative not excluded"
            ),
            (ReviewSeverity::Minor, "measure", "Label wording"),
        ]
    );
    assert_eq!(review.failed_reviewers.len(), 1);
    assert!(review.failed_reviewers[0].starts_with("coverage: invalid structured review"));
    let text = summary(review);
    assert!(text.contains("1 blocking, 1 major and 1 minor"));
    assert!(text.contains("1 reviewer(s) did not return"));
}

fn real_finding(
    lens: &str,
    severity: ReviewSeverity,
    title: &str,
    detail: &str,
    evidence: &str,
) -> ReviewIssue {
    ReviewIssue {
        lens: lens.into(),
        severity,
        title: title.into(),
        detail: detail.into(),
        evidence: evidence.into(),
    }
}

/// Findings from a real reviewed workflow (ultros_prefill_transfer, review 1):
/// three lenses restating one count error, two restating one wire-capture
/// caveat, and two distinct findings that share topic words.
fn prefill_findings() -> Vec<ReviewIssue> {
    use ReviewSeverity::Major;
    use ReviewSeverity::Minor;
    vec![
        real_finding(
            "coverage",
            Major,
            "The MemorySandbox sentence reverses the admission count",
            "“Admitted no document on 0 of 8 tasks” says the reader returned nothing zero times. The parenthetical gives the opposite and correct result. Use “ranked the desk register first on 3/4 current assignments and admitted zero documents across all eight tasks.”",
            "Joining reader-check/reader/selections.json to reader-check/source-manifest.json and lookup-fixture/manifest.json yielded an empty admittedSourceRefs array in each of the eight learned-reader rows. Field-desk.log ranked first in assigned sessions 001, 004 and 005. Bench-log.txt ranked first in assigned session 008.",
        ),
        real_finding(
            "measure",
            Minor,
            "The admission sentence states opposite results",
            "“Admitted no document on 0 of 8 tasks” means the reader abstained zero times. The parenthesis gives the correct result. Replace the clause with “It admitted a document on 0/8 tasks.”",
            "All eight learned-reader entries have an empty admittedSourceRefs array and below-fixed-gate abstention. The desk register ranks first on three of the four assigned tasks. [Reader selections](C:/Users/phrop/Neuromorphy/runs/release-transfer-20261117-v1/reader-check/reader/selections.json)",
        ),
        real_finding(
            "sufficiency",
            Minor,
            "The MemorySandbox sentence reverses the abstention count",
            "Replace “admitted no document on 0 of 8 tasks (zero admissions in eight)” with “admitted a document on 0/8 tasks” or “abstained on all eight.” The two clauses currently assert opposite outcomes.",
            "All eight rows for the learned reader have an empty admittedSourceRefs array in the [raw reader selections](C:/Users/phrop/Neuromorphy/runs/release-transfer-20261117-v1/reader-check/reader/selections.json).",
        ),
        real_finding(
            "measure",
            Minor,
            "The three-request wire capture used a separate sentinel",
            "After identifying the original field-desk dispatch, “the exact record appeared in all three subsequent model requests” can imply that a live-study dispatch was the wire-test payload. Say “the exact test marker appeared in all three requests.” The separate live runs demonstrate dispatch use through the private tickets in their briefs.",
            "All three wire items match the insertion acknowledgment byte-for-byte in content and carry ferry marker R-4fa9642c. The four live insertions instead carry project-specific D- tickets. [Wire-test insertion](C:/Users/phrop/Neuromorphy/runs/prefill-wire-20261117-v2/history-insertion.json) · [Live alder insertion](C:/Users/phrop/Neuromorphy/runs/release-transfer-20261117-v1/native-live/006/actor/history-insertion.json)",
        ),
        real_finding(
            "sufficiency",
            Minor,
            "The wire capture used a separate canary record",
            "“The exact record appeared in all three subsequent model requests” reads as though the recording endpoint captured the field-desk dispatch from a live release run. It captured the exact bytes of a separate ferry-desk canary. State that directly. The live dispatch claim has separate evidence in four acknowledged insertions and four briefs quoting the corresponding tickets.",
            "All three [captured requests](C:/Users/phrop/Neuromorphy/runs/prefill-wire-20261117-v2/model-inputs.json) contain R-4fa9642c under source study:prefill-transport. I compared the four live insertion items with the corresponding delivered briefs. Each brief contains its own dispatch ticket, D-442daec5, D-aceaad1e, D-cf45e82f or D-6ee676c0.",
        ),
        real_finding(
            "measure",
            Minor,
            "The native comparison also changed the model-visible advisory fields",
            "The draft names repeated visibility and position as differences in the retained treatment. The unchanged dispatch and evidentiary warning also sat inside different JSON envelopes. The one-shot native message carried a proposal ID and a context object, while the retained message omitted both. The measured advantage belongs to the tested presentation as a whole. The data do not identify whether the extra fields contributed.",
            "The staged client serializes the complete Advisory containing proposal_id, text, source_refs and context. The prefill adapter serializes only text and sourceRefs. The recorded bridge response for native row 001 and the insertion acknowledgment for row 013 confirm the difference for the same larch dispatch.",
        ),
        real_finding(
            "coverage",
            Minor,
            "The one-shot table obscures the nature of Bonsai’s apparent advantage",
            "“Correct brief” gives Bonsai 2/4 versus Sol 0/4 in each one-shot condition. Both Bonsai matches followed the same project default it chose when the assignment was withheld. On the other two assignments per condition, that behavior actively cleared the wrong profile. Sol’s one-shot files left authority unresolved. The Bellweir byte-identity example establishes part of this pattern, but the headline comparison should distinguish matching the assignment from evidence that the actor used it and give the wrong-clearance count.",
            "Matching the eight one-shot Bonsai files against fixture/private/expected.json shows that every Bellweir file cleared sedge and every Loomgate file cleared alder. Sessions 001, 002, 005 and 014 cleared a profile different from the assigned one. The two withheld files chose those same defaults. All four Bellweir one-shot file hashes matched the withheld hash.",
        ),
    ]
}

#[test]
fn acceptance_review_merges_real_restatements_but_keeps_distinct_findings() {
    let merged = merge_duplicates(prefill_findings());
    let lenses = merged
        .iter()
        .map(|issue| issue.lens.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        lenses,
        vec![
            "coverage+measure+sufficiency",
            "measure+sufficiency",
            "measure",
            "coverage"
        ],
        "{merged:#?}"
    );
    let count = &merged[0];
    // The most severe report sets severity; every lens's title and text survive.
    assert_eq!(count.severity, ReviewSeverity::Major);
    assert_eq!(
        count.title,
        "The MemorySandbox sentence reverses the admission count"
    );
    for text in [
        "[coverage lens] The MemorySandbox sentence reverses the admission count:",
        "[measure lens] The admission sentence states opposite results:",
        "[sufficiency lens] The MemorySandbox sentence reverses the abstention count:",
    ] {
        assert!(count.detail.contains(text), "missing {text}");
    }
    assert!(
        count
            .evidence
            .contains("[coverage lens] Joining reader-check")
    );
    assert!(
        count
            .evidence
            .contains("[measure lens] All eight learned-reader")
    );
}

#[test]
fn acceptance_review_never_merges_findings_from_the_same_lens() {
    let mut findings = prefill_findings();
    let mut restated = findings[1].clone();
    restated.title.push_str(" again");
    findings.insert(2, restated);
    let merged = merge_duplicates(findings);
    // The measure lens's near-identical restatement stays its own issue.
    assert_eq!(merged[0].lens, "coverage+measure+sufficiency");
    assert_eq!(merged[1].lens, "measure");
    assert!(merged[1].title.ends_with(" again"));
}

#[test]
fn acceptance_review_answering_never_resumes_a_stopped_workflow() {
    for status in [
        RunStatus::Cancelled,
        RunStatus::Paused,
        RunStatus::NeedsAttention,
    ] {
        let mut state = returned_workflow();
        begin_review(&mut state);
        returned_review(&mut state, json!([issue("blocking", "Gap")]));
        state.status = status;
        let before = state.clone();
        for final_answer in [DRAFT, "CHANGED ANSWER"] {
            let error =
                answer(&mut state, "handled", Some("Corrected"), Some(final_answer)).unwrap_err();
            assert!(error.contains("Resume it before answering"), "{error}");
            assert_eq!(state, before);
        }
    }
}

#[test]
fn acceptance_review_delta_loops_are_bounded_and_respect_the_review_switch() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    returned_review(&mut state, json!([]));
    for round in 0..MAX_DELTA_REVIEWS {
        let specs = answer(
            &mut state,
            "handled",
            None,
            Some(&format!("CHANGED ANSWER {round}")),
        )
        .unwrap()
        .unwrap();
        complete(&mut state, &specs[0].id, &review_output(json!([])));
        assert!(collect(&mut state));
        state.status = RunStatus::AwaitingDecision;
    }
    let error = answer(&mut state, "handled", None, Some("YET ANOTHER")).unwrap_err();
    assert!(error.contains("Deliver the last reviewed text unchanged"));
    // The last reviewed text still completes the workflow.
    let last = format!("CHANGED ANSWER {}", MAX_DELTA_REVIEWS - 1);
    assert!(
        answer(&mut state, "handled", None, Some(&last))
            .unwrap()
            .is_none()
    );
    assert_eq!(state.status, RunStatus::Completed);
    // With review switched off, a changed answer is recorded and completes.
    let mut state = returned_workflow();
    begin_review(&mut state);
    returned_review(&mut state, json!([]));
    assert!(
        respond(&mut state, "handled", None, Some("CHANGED"), false, None)
            .unwrap()
            .is_none()
    );
    assert_eq!(state.status, RunStatus::Completed);
}

#[test]
fn acceptance_review_delta_reviewers_read_the_users_current_messages_and_full_round() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    returned_review(&mut state, json!([issue("major", "Panel finding")]));
    let fresh = (vec!["USER: also cover the travel router".to_string()], None);
    let first = respond(
        &mut state,
        "handled",
        Some("Panel response"),
        Some("FINAL ONE"),
        true,
        Some(fresh.clone()),
    )
    .unwrap()
    .unwrap();
    assert_eq!(active(&state).unwrap().user_messages, fresh.0);
    // The first delta fails twice; the second delta still sees the whole round.
    let task = state.tasks.get_mut(&first[0].id).unwrap();
    task.status = TaskStatus::Failed;
    task.attempt = 2;
    assert!(collect(&mut state));
    state.status = RunStatus::AwaitingDecision;
    let second = answer(
        &mut state,
        "handled",
        Some("Delta reviewer failed twice"),
        Some("FINAL TWO"),
    )
    .unwrap()
    .unwrap();
    let prompt = super::super::task_prompt(&state, &second[0].id, std::path::Path::new("records"));
    assert!(prompt.contains("## Draft reviewed by the panel\nDRAFT:"));
    assert!(prompt.contains("[Major/measure] Panel finding"));
    assert!(prompt.contains("Panel response"));
    assert!(prompt.contains("treat what they were to check as unreviewed"));
    assert!(
        prompt.contains("## Text before this change (last reviewed by a delta review)\nFINAL ONE")
    );
    assert!(prompt.contains("[ADDED OR REWRITTEN]\nFINAL TWO"));
}

#[test]
fn acceptance_review_reviewer_identity_comes_from_the_agent_path() {
    use super::super::is_reviewer_agent_path;
    use super::super::native_task_name;
    for lens in LENS_KEYS.iter().chain(["delta"].iter()) {
        let name = native_task_name("ultros_prefill", &format!("_review_3_{lens}"), 2);
        assert!(is_reviewer_agent_path(&format!("/root/{name}")), "{name}");
    }
    for task in ["capture", "_design_1", "x_review_y", "review_1_measure"] {
        let name = native_task_name("review-test", task, 1);
        assert!(!is_reviewer_agent_path(&format!("/root/{name}")), "{name}");
    }
    assert!(!is_reviewer_agent_path("/root"));
}

#[test]
fn acceptance_review_findings_require_a_recorded_response_but_do_not_veto() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    returned_review(
        &mut state,
        json!([issue("blocking", "Capture stopped on a clean window")]),
    );
    let before = state.clone();
    let error = answer(&mut state, "handled", None, Some(DRAFT)).unwrap_err();
    assert!(error.contains("1 blocking"));
    assert!(error.contains("review_response"));
    let error = answer(&mut state, "handled", Some("Corrected"), None).unwrap_err();
    assert!(error.contains("final_answer is required"));
    assert_eq!(state, before);
    // Delivering exactly the reviewed text completes the workflow.
    let done = answer(
        &mut state,
        "The path remains unproven; the answer now says so",
        Some("Accepted risk: the capture stays as reported."),
        Some(&format!("  {DRAFT}\n")),
    )
    .unwrap();
    assert!(done.is_none());
    assert_eq!(state.status, RunStatus::Completed);
    let design = state.design.as_ref().unwrap();
    assert!(design.assessment.as_deref().unwrap().contains("unproven"));
    assert!(
        design.reviews[0]
            .response
            .as_deref()
            .unwrap()
            .contains("Accepted risk")
    );
    assert_eq!(design.reviews[0].final_answer.as_deref(), Some(DRAFT));
    assert!(awaiting_response(&state).is_none());
}

#[test]
fn acceptance_review_a_changed_final_answer_gets_a_delta_review() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    returned_review(
        &mut state,
        json!([issue("blocking", "Capture stopped on a clean window")]),
    );
    let final_answer = "FINAL: the path is not ruled out; the capture stays armed until a failure is recorded. NEW RESULT: 6 extra sessions confirm it.";
    let specs = answer(
        &mut state,
        "Corrected the stopping rule",
        Some("Corrected: the final answer keeps the capture armed."),
        Some(final_answer),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        specs
            .iter()
            .map(|spec| spec.id.as_str())
            .collect::<Vec<_>>(),
        vec!["_review_2_delta"]
    );
    assert_eq!(state.status, RunStatus::Running);
    let delta = active(&state).unwrap();
    assert_eq!(delta.kind, ReviewKind::Delta);
    assert_eq!(delta.draft_answer, final_answer);
    assert_eq!(
        delta.user_messages[0],
        "USER: keep capturing until a bad-MAC failure is recorded"
    );
    let first = &state.design.as_ref().unwrap().reviews[0];
    assert!(first.response.as_deref().unwrap().contains("capture armed"));
    assert_eq!(first.final_answer.as_deref(), Some(final_answer));
    let prompt =
        super::super::task_prompt(&state, "_review_2_delta", std::path::Path::new("records"));
    assert!(prompt.contains("Changes since the reviewed draft"));
    assert!(
        prompt.contains("## Draft reviewed by the panel\nDRAFT: the network path is ruled out")
    );
    assert!(prompt.contains("## Paragraphs changed since that text"));
    assert!(prompt.contains("[REMOVED]\nDRAFT:"));
    assert!(prompt.contains("[ADDED OR REWRITTEN]\nFINAL:"));
    assert!(prompt.contains("[Blocking/measure] Capture stopped on a clean window"));
    assert!(prompt.contains("Corrected: the final answer keeps the capture armed."));
    assert!(prompt.contains("## FINAL ANSWER UNDER REVIEW (to be delivered)\nFINAL:"));
    assert!(prompt.contains("NEW RESULT: 6 extra sessions"));
    // The delta returns; delivering the reviewed final answer completes.
    complete(
        &mut state,
        "_review_2_delta",
        &review_output(json!([issue("minor", "Name the session IDs")])),
    );
    assert!(collect(&mut state));
    state.status = RunStatus::AwaitingDecision;
    assert!(
        coordinator_hint(&state, true)
            .unwrap()
            .starts_with("Delta review of the final answer 2")
    );
    assert!(
        answer(&mut state, "Handled", None, Some(final_answer))
            .unwrap()
            .is_none()
    );
    assert_eq!(state.status, RunStatus::Completed);
    let reviews = &state.design.as_ref().unwrap().reviews;
    assert_eq!(reviews.len(), 2);
    assert_eq!(reviews[1].final_answer.as_deref(), Some(final_answer));
}

#[test]
fn acceptance_review_record_format_stays_readable() {
    // Records written by the first review build have no kind or final_answer.
    let legacy = json!({
        "round":1,"attempt":1,"assessment":"a","draft_answer":"d","user_messages":[],
        "standing_instructions":null,"task_ids":[],"complete":true,"issues":[],
        "failed_reviewers":[],"superseded":false,"response":"r"
    });
    let record: ReviewRecord = serde_json::from_value(legacy).unwrap();
    assert_eq!(record.kind, ReviewKind::Full);
    assert_eq!(record.final_answer, None);
    // Defaults are omitted when written; a delta record says what it is.
    let written = serde_json::to_value(&record).unwrap();
    assert!(written.get("kind").is_none());
    assert!(written.get("final_answer").is_none());
    let delta = ReviewRecord {
        kind: ReviewKind::Delta,
        ..record
    };
    assert_eq!(serde_json::to_value(&delta).unwrap()["kind"], "delta");
}

#[test]
fn acceptance_review_with_only_minor_findings_finishes_without_a_response() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    for lens in LENS_KEYS {
        complete(
            &mut state,
            &format!("_review_1_{lens}"),
            &review_output(json!([issue("minor", &format!("{lens} wording"))])),
        );
    }
    collect(&mut state);
    state.status = RunStatus::AwaitingDecision;
    answer(
        &mut state,
        "Requested diagnosis is complete",
        None,
        Some(DRAFT),
    )
    .unwrap();
    assert_eq!(state.status, RunStatus::Completed);
}

#[test]
fn acceptance_review_findings_feed_advance_and_the_next_draft_is_reviewed_afresh() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    returned_review(
        &mut state,
        json!([issue("blocking", "Capture stopped on a clean window")]),
    );
    let planner =
        planning::advance(&mut state, "Keep capturing until a failure is recorded").unwrap();
    // The planner runs after work tasks only, never after reviewers.
    assert_eq!(planner.depends_on, vec!["capture"]);
    assert!(planner.prompt.contains("Capture stopped on a clean window"));
    assert!(planner.prompt.contains("1 blocking"));
    assert!(active(&state).is_none());
    assert!(state.design.as_ref().unwrap().reviews[0].superseded);
    // The next round's answer gets its own review with distinct task IDs.
    complete(&mut state, "_design_2", "DESIGN RECEIPT 2");
    planning::apply_design(
        &mut state,
        "_design_2",
        &json!({"strategy":"Capture a failure","completion_criteria":["Record a failing session"],"deferred_decisions":[],"tasks":[{"id":"armed_capture","group":"Evidence","role":"Capture","prompt":"Capture until failure","deliverable":"Failing capture","inputs":[{"task_id":"capture","use_for":"Reuse the capture filters"}]}]}).to_string(),
    )
    .unwrap();
    complete(&mut state, "armed_capture", "FAILURE CAPTURED");
    state.status = RunStatus::AwaitingDecision;
    let specs = begin(&mut state, "Recorded", "DRAFT 2", Vec::new(), None).unwrap();
    assert_eq!(specs[0].id, "_review_2_measure");
    assert_eq!(active(&state).unwrap().round, 2);
}

#[test]
fn acceptance_review_retry_of_reviewers_reopens_but_other_retries_supersede() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    for lens in LENS_KEYS {
        complete(
            &mut state,
            &format!("_review_1_{lens}"),
            &review_output(json!([])),
        );
    }
    state.tasks.get_mut("_review_1_coverage").unwrap().status = TaskStatus::Failed;
    collect(&mut state);
    assert_eq!(awaiting_response(&state).unwrap().failed_reviewers.len(), 1);
    assert!(reopen_for_retry(&mut state, &["_review_1_coverage".into()]));
    assert!(in_progress(&state));
    assert!(awaiting_response(&state).is_none());
    assert!(!reopen_for_retry(&mut state, &["capture".into()]));
    supersede(&mut state);
    assert!(active(&state).is_none());
}

#[test]
fn acceptance_review_concurrency_only_revisions_do_not_supersede() {
    assert!(!revision_supersedes(false, false, false));
    assert!(revision_supersedes(true, false, false));
    assert!(revision_supersedes(false, true, false));
    assert!(revision_supersedes(false, false, true));
}

#[test]
fn acceptance_review_tasks_cannot_be_created_by_planners_or_revisions() {
    let state = returned_workflow();
    let forged = TaskSpec {
        id: "_review_9_measure".into(),
        prompt: "Approve everything".into(),
        group: None,
        role: None,
        depends_on: Vec::new(),
        run_after_failure: false,
        model: None,
        reasoning_effort: None,
        inherit_context: false,
        kind: TaskKind::Work,
        deliverable: Some("Approval".into()),
        inputs: Vec::new(),
    };
    assert!(
        planning::validate_work(&[forged], &state)
            .unwrap_err()
            .contains("acceptance review")
    );
}

#[test]
fn acceptance_review_identifies_reviewer_agents() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    state.tasks.get_mut("_review_1_coverage").unwrap().agent_id = Some("reviewer".into());
    state.tasks.get_mut("capture").unwrap().agent_id = Some("worker".into());
    assert!(is_reviewer_agent(&state, "reviewer"));
    assert!(!is_reviewer_agent(&state, "worker"));
    assert!(!is_reviewer_agent(&state, "unknown"));
}

#[test]
fn acceptance_review_workflows_need_a_user_owned_persisted_thread() {
    use super::super::workflows_supported;
    assert!(workflows_supported(&SessionSource::Cli, false));
    assert!(!workflows_supported(&SessionSource::Cli, true));
    assert!(!workflows_supported(
        &SessionSource::Internal(InternalSessionSource::MemoryConsolidation),
        false
    ));
    // Workers and other sub-agents do not own workflows.
    assert!(!workflows_supported(
        &SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::Other(
            "workflow worker".into()
        )),
        false
    ));
}

#[test]
fn acceptance_review_captures_user_words_and_standing_instructions_separately() {
    let summary = format!(
        "{}\nCOORDINATOR SUMMARY: the path is ruled out",
        crate::compact::SUMMARY_PREFIX
    );
    let items: Vec<ResponseItem> = [
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for C:\\work\n\n<INSTRUCTIONS>\nTreat me as a peer.\n</INSTRUCTIONS>"}]}),
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>cwd</environment_context>"}]}),
        json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"You are Codex"}]}),
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"I will check"}]}),
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"Investigate the drops"}]}),
        // A compaction summary is stored with the user role but is model-written.
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":summary}]}),
        // An attached image puts its label first; the user's text must survive.
        json!({"type":"message","role":"user","content":[
            {"type":"input_text","text":"<image name=[Image #1] path=\"print.png\">"},
            {"type":"input_image","image_url":"data:image/png;base64,AAAA"},
            {"type":"input_text","text":"</image>"},
            {"type":"input_text","text":"Why does this print fail?"}
        ]}),
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"<b>Bold</b> also check the video cache failures"}]}),
    ]
    .into_iter()
    .map(|value| serde_json::from_value(value).unwrap())
    .collect();
    let (messages, standing) = user_messages(items.iter());
    // The compaction summary is excluded, and its presence is noted first.
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert_eq!(messages[0], COMPACTED_NOTE);
    assert_eq!(messages[1], "Investigate the drops");
    assert!(messages[2].starts_with("Why does this print fail?"));
    assert!(messages[2].contains("attached 1 non-text input"));
    assert_eq!(
        messages[3],
        "<b>Bold</b> also check the video cache failures"
    );
    assert!(
        messages
            .iter()
            .all(|message| !message.contains("COORDINATOR SUMMARY"))
    );
    assert!(standing.unwrap().contains("Treat me as a peer."));
}

#[test]
fn acceptance_review_interrupted_reviewers_keep_the_review_open_until_retried() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    for lens in ["measure", "sufficiency", "intent"] {
        complete(
            &mut state,
            &format!("_review_1_{lens}"),
            &review_output(json!([])),
        );
    }
    // Pause, owner interrupt or restart recovery leaves a reviewer Interrupted.
    state.tasks.get_mut("_review_1_coverage").unwrap().status = TaskStatus::Interrupted;
    assert!(!collect(&mut state));
    assert!(in_progress(&state));
    assert!(awaiting_response(&state).is_none());
    let stalled = stalled_message(&state).unwrap();
    assert!(stalled.contains("_review_1_coverage"));
    assert!(stalled.contains("retry"));
    state.status = RunStatus::NeedsAttention;
    assert!(
        coordinator_hint(&state, true)
            .unwrap()
            .contains("_review_1_coverage")
    );
    let error = begin(&mut state, "done", "answer", Vec::new(), None).unwrap_err();
    assert!(error.contains("_review_1_coverage"));
    // Retrying exactly the stopped reviewer reopens the same review.
    assert!(reopen_for_retry(&mut state, &["_review_1_coverage".into()]));
    complete(&mut state, "_review_1_coverage", &review_output(json!([])));
    assert!(collect(&mut state));
    assert!(awaiting_response(&state).is_some());
}

#[test]
fn acceptance_review_with_no_usable_reviewer_requires_a_retry_first() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    for lens in LENS_KEYS {
        complete(&mut state, &format!("_review_1_{lens}"), "not structured");
    }
    collect(&mut state);
    state.status = RunStatus::AwaitingDecision;
    let error = answer(&mut state, "handled", Some("Reviewers failed"), Some(DRAFT)).unwrap_err();
    assert!(error.contains("No reviewer returned a usable review"));
    // After a repeated failure, a recorded response may stand in for the review.
    for lens in LENS_KEYS {
        state
            .tasks
            .get_mut(&format!("_review_1_{lens}"))
            .unwrap()
            .attempt = 2;
    }
    answer(
        &mut state,
        "handled",
        Some("Reviewers failed twice; verified the capture directly"),
        Some(DRAFT),
    )
    .unwrap();
    assert_eq!(state.status, RunStatus::Completed);
}

#[test]
fn acceptance_review_superseded_reviewers_release_capacity() {
    let mut state = returned_workflow();
    begin_review(&mut state);
    let running = state.tasks.get_mut("_review_1_measure").unwrap();
    running.status = TaskStatus::Running;
    running.agent_id = Some("reviewer-agent".into());
    assert_eq!(supersede(&mut state), vec!["reviewer-agent".to_string()]);
    for lens in &LENS_KEYS[1..] {
        assert_eq!(
            state.tasks[&format!("_review_1_{lens}")].status,
            TaskStatus::Cancelled
        );
    }
    assert!(active(&state).is_none());
    assert!(
        !model::ready_tasks(&state)
            .iter()
            .any(|id| is_review_task(id))
    );
}

#[test]
fn acceptance_review_hints_follow_the_run_status() {
    let mut state = returned_workflow();
    assert!(
        coordinator_hint(&state, true)
            .unwrap()
            .contains("draft_answer")
    );
    assert!(coordinator_hint(&state, false).is_none());
    begin_review(&mut state);
    let waiting = coordinator_hint(&state, true).unwrap();
    assert!(waiting.contains("are checking"));
    assert!(waiting.contains("Do not poll"));
    state.status = RunStatus::Paused;
    assert!(coordinator_hint(&state, true).unwrap().contains("resume"));
    for lens in LENS_KEYS {
        complete(
            &mut state,
            &format!("_review_1_{lens}"),
            &review_output(json!([issue("major", &format!("{lens} gap"))])),
        );
    }
    collect(&mut state);
    assert!(
        coordinator_hint(&state, true)
            .unwrap()
            .contains("resume it")
    );
    state.status = RunStatus::AwaitingDecision;
    let answer = coordinator_hint(&state, true).unwrap();
    assert!(answer.contains("review_response"));
    assert!(answer.contains("final_answer"));
    state.status = RunStatus::Cancelled;
    assert!(coordinator_hint(&state, true).is_none());
}

#[test]
fn acceptance_review_does_not_consume_the_work_task_allowance() {
    let mut state = returned_workflow();
    let filler = (0..127)
        .map(|index| TaskSpec {
            id: format!("work-{index}"),
            prompt: "Collect evidence".into(),
            group: None,
            role: None,
            depends_on: Vec::new(),
            run_after_failure: false,
            model: None,
            reasoning_effort: None,
            inherit_context: false,
            kind: TaskKind::Work,
            deliverable: Some("Evidence".into()),
            inputs: Vec::new(),
        })
        .collect::<Vec<_>>();
    // _design_1 + capture + 126 more = the 128-task work allowance.
    model::revise_tasks_with_removals(&mut state, filler[..126].to_vec(), &[]).unwrap();
    for id in filler[..126].iter().map(|task| task.id.clone()) {
        complete(&mut state, &id, "done");
    }
    state.status = RunStatus::AwaitingDecision;
    begin_review(&mut state);
    assert_eq!(state.plan.tasks.len(), 132);
    assert!(
        model::revise_tasks_with_removals(&mut state, filler[126..].to_vec(), &[])
            .unwrap_err()
            .contains("between 1 and 128")
    );
}

#[test]
fn acceptance_review_clipping_keeps_report_conclusions() {
    let text = format!("START{}CONCLUSION", "x".repeat(1_000));
    let clipped = clip(&text, 300);
    assert!(clipped.starts_with("START"));
    assert!(clipped.ends_with("CONCLUSION"));
    assert!(clipped.contains("characters omitted"));
    assert_eq!(clip("short", 300), "short");
}

#[test]
fn acceptance_review_state_stays_compatible_with_unreviewed_journals() {
    let state = returned_workflow();
    let value = serde_json::to_value(&state).unwrap();
    assert!(value["design"].get("reviews").is_none());
    let restored: WorkflowState = serde_json::from_value(value).unwrap();
    assert_eq!(restored, state);
    let mut reviewed = returned_workflow();
    begin_review(&mut reviewed);
    let restored: WorkflowState =
        serde_json::from_value(serde_json::to_value(&reviewed).unwrap()).unwrap();
    assert_eq!(restored, reviewed);
    model::validate_state(&restored).unwrap();
}
