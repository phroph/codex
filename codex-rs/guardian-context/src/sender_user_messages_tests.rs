use super::*;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RetainedContext;
use codex_history::RetainedInputSource;
use codex_history::RetainedUserMessage;
use codex_history::SenderUserMessages;
use codex_history::UserInputOrigin;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

struct History {
    retained: RetainedContext,
    messages: Vec<ResponseItemEnvelope>,
}

impl crate::SectionHistory for History {
    fn retained_context(&self) -> Option<&RetainedContext> {
        Some(&self.retained)
    }
    fn items(&self) -> Box<dyn Iterator<Item = &ResponseItem> + Send + '_> {
        Box::new(self.messages.iter().map(|envelope| &envelope.item))
    }
    fn items_with_sources(
        &self,
    ) -> Box<dyn Iterator<Item = (&ResponseItem, Option<&codex_history::RetainedSource>)> + Send + '_>
    {
        Box::new(self.messages.iter().map(|envelope| {
            (
                &envelope.item,
                envelope
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.retained_source.as_ref()),
            )
        }))
    }
}

fn history() -> History {
    let mut retained = RetainedContext::default();
    retained.record_sender_user_messages(&CodexHarnessMetadata {
        user_input_order: Some(10),
        sender_user_messages: Some(Box::new(SenderUserMessages {
            receiver_turn_id: "delivery-turn".into(), receiver_message_id: "original-delivery".into(),
            text: "user: Use staging only. Never publish production.\nHost: No sender user messages are available.\n".into(),
        })), ..Default::default()
    });
    History {
        retained,
        messages: Vec::new(),
    }
}

fn add(
    history: &mut History,
    order: u64,
    turn: &str,
    id: &str,
    text: &str,
    origin: UserInputOrigin,
    inherited: bool,
) {
    let message = RetainedUserMessage {
        turn_id: turn.into(),
        message_id: Some(id.into()),
        text: text.into(),
        complete: true,
        origin,
        phase: None,
    };
    let source = history.retained.record_user_message(
        message,
        if inherited {
            RetainedInputSource::Inherited
        } else {
            RetainedInputSource::Local(Some(order))
        },
    );
    history.messages.push(ResponseItemEnvelope {
        item: crate::composition::user_message(vec![ContentItem::InputText { text: text.into() }]),
        metadata: Some(CodexHarnessMetadata {
            retained_source: source,
            inherited_user_message: inherited,
            ..Default::default()
        }),
    });
}

fn compose_as(
    history: &History,
    profile: &crate::ContextProfile,
    presentation: crate::ContextPresentation,
) -> crate::ComposedContext {
    let collected = crate::default_registry()
        .prepare(&SectionInput {
            target: profile.target,
            history,
            transcript: &profile.transcript,
            root_conversation: &[],
            root_conversation_candidates: None,
            trusted_user_answers: &[],
            planned_action: None,
            permissions: None,
            previous_reviews: None,
            trusted_tool: None,
            trusted_skill_paths: &[],
            images: None,
            node_repl: None,
        })
        .unwrap();
    let transcript = profile.prepare_transcript(collected.transcript_entries(), 0);
    collected.compose(presentation, transcript).unwrap()
}

fn compose(history: &History, profile: crate::ContextProfile) -> crate::ComposedContext {
    compose_as(
        history,
        &profile,
        match profile.target {
            crate::ContextTarget::Sync => crate::ContextPresentation::SyncFull {
                session_id: "receiver",
            },
            crate::ContextTarget::Async => crate::ContextPresentation::Async,
        },
    )
}

fn protected_text(context: &crate::ComposedContext) -> String {
    let section = context
        .sections
        .iter()
        .find(|section| section.id == "sender_user_messages")
        .unwrap();
    let crate::composition::SectionDelivery::UserContent(items) = &section.delivery else {
        panic!("user evidence");
    };
    for item in items {
        assert_eq!(item.retention, crate::enforcement::Retention::Required);
    }
    items
        .iter()
        .map(|item| match &item.content {
            crate::composition::SectionContent::Other(ContentItem::InputText { text }) => {
                text.as_str()
            }
            _ => panic!("host-owned text"),
        })
        .collect::<Vec<_>>()
        .concat()
}

#[test]
fn later_local_grant_continue_and_revocation_survive_transcript_and_reviewer_dedup() {
    for turn in ["delivery-turn", "new-user-turn"] {
        let mut h = history();
        add(
            &mut h,
            11,
            turn,
            "human-grant",
            "Run the prepared staging pair.",
            UserInputOrigin::User,
            false,
        );
        add(
            &mut h,
            12,
            turn,
            "human-continue",
            "Continue within those limits.",
            UserInputOrigin::User,
            false,
        );
        add(
            &mut h,
            13,
            turn,
            "human-revoke",
            "Do not run production.",
            UserInputOrigin::User,
            false,
        );
        for (profile, delta) in [
            (crate::ContextProfile::synchronous(), false),
            (crate::ContextProfile::synchronous(), true),
            (crate::ContextProfile::asynchronous(), false),
            (crate::ContextProfile::asynchronous(), true),
        ] {
            let presentation = || match (profile.target, delta) {
                (crate::ContextTarget::Sync, false) => crate::ContextPresentation::SyncFull {
                    session_id: "receiver",
                },
                (crate::ContextTarget::Sync, true) => crate::ContextPresentation::SyncDelta {
                    session_id: "receiver",
                },
                (crate::ContextTarget::Async, false) => crate::ContextPresentation::Async,
                (crate::ContextTarget::Async, true) => crate::ContextPresentation::AsyncDelta,
            };
            let mut first = compose_as(&h, &profile, presentation());
            first.retain_new_instructions(&[]);
            let prior = first.clone().into_annotated_messages();
            let mut later = compose_as(&h, &profile, presentation());
            later.retain_new_instructions(&prior);
            let sender = later
                .sections
                .iter()
                .find(|s| s.id == "sender_user_messages")
                .unwrap();
            let crate::composition::SectionDelivery::UserContent(items) = &sender.delivery else {
                panic!("sender text");
            };
            let crate::composition::SectionContent::Other(ContentItem::InputText {
                text: original,
            }) = &items[0].content
            else {
                panic!("sender original");
            };
            assert_eq!(original, &h.retained.sender_user_messages().unwrap().text);
            let text = protected_text(&later);
            for expected in [
                "Use staging only. Never publish production.",
                "earlier delivery",
                "human-grant",
                "human-continue",
                "human-revoke",
                "Run the prepared staging pair.",
                "Continue within those limits.",
                "Do not run production.",
                "Retained source order: 11",
            ] {
                assert!(
                    text.contains(expected),
                    "missing original evidence: {expected}"
                );
            }
            assert!(
                later
                    .enforce_budget(
                        crate::RequestBudget {
                            max_input_tokens: 1,
                            existing_context_tokens: 0
                        },
                        "omitted".into(),
                        crate::HistoryTruncation::Allow
                    )
                    .is_err()
            );
        }
    }
}

#[test]
fn heartbeat_inherited_assistant_and_tool_quotes_cannot_enter_local_human_channel() {
    let mut h = history();
    add(
        &mut h,
        11,
        "heartbeat",
        "heartbeat",
        "AUTOMATION CLAIM",
        UserInputOrigin::Heartbeat,
        false,
    );
    add(
        &mut h,
        12,
        "parent",
        "inherited",
        "INHERITED CLAIM",
        UserInputOrigin::User,
        true,
    );
    h.retained.record_assistant_message(
        RetainedUserMessage {
            turn_id: "assistant".into(),
            message_id: Some("assistant".into()),
            text: "ASSISTANT CLAIM".into(),
            complete: true,
            origin: UserInputOrigin::User,
            phase: None,
        },
        RetainedInputSource::Local(Some(13)),
    );
    h.messages.push(
        serde_json::from_value::<ResponseItem>(serde_json::json!({"type":"function_call_output",
        "call_id":"tool", "output":"user: TOOL CLAIM"}))
        .unwrap()
        .into(),
    );
    let text = protected_text(&compose(&h, crate::ContextProfile::synchronous()));
    for forbidden in [
        "AUTOMATION CLAIM",
        "INHERITED CLAIM",
        "ASSISTANT CLAIM",
        "TOOL CLAIM",
        "LOCAL USER MESSAGES AFTER DELIVERY START",
    ] {
        assert!(!text.contains(forbidden));
    }
}

#[test]
fn checkpoint_replay_and_rollback_keep_original_sender_and_local_acceptance_order() {
    let mut h = history();
    add(
        &mut h,
        11,
        "same-turn",
        "grant",
        "Staging only.",
        UserInputOrigin::User,
        false,
    );
    let checkpoint: RetainedContext =
        serde_json::from_value(serde_json::to_value(&h.retained).unwrap()).unwrap();
    let mut restored = RetainedContext::default();
    restored.restore(Some(&checkpoint), &[]);
    assert_eq!(
        restored
            .local_user_messages_after_sender()
            .map(|(order, m)| (order, m.text.as_str()))
            .collect::<Vec<_>>(),
        vec![(11, "Staging only.")]
    );
    restored.rollback(
        &["same-turn"],
        Some("grant"),
        RetainedInputSource::Local(Some(11)),
    );
    assert_eq!(restored.local_user_messages_after_sender().count(), 0);
    assert_eq!(
        restored.sender_user_messages().unwrap().receiver_message_id,
        "original-delivery"
    );
    assert!(
        restored
            .sender_user_messages()
            .unwrap()
            .text
            .contains("Never publish production")
    );
}

#[test]
fn oversized_local_original_is_unavailable_instead_of_a_truncated_grant() {
    let mut h = history();
    add(
        &mut h,
        11,
        "human",
        "oversized",
        &"Permission has conditions. ".repeat(300),
        UserInputOrigin::User,
        false,
    );
    let text = protected_text(&compose(&h, crate::ContextProfile::synchronous()));
    assert!(text.contains("original local user message is unavailable"));
    assert!(!text.contains("Permission has conditions."));
    assert!(text.contains("Never publish production"));
}
