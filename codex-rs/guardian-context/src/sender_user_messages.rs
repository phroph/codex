//! Host-owned sender history and subsequent original local-human instructions.
//! Both remain mandatory; neither forwarded text nor tool output gains authority.

use crate::ContextSection;
use crate::GuardianRootMessage;
use crate::SectionContributor;
use crate::SectionError;
use crate::SectionInput;
use crate::SectionScope;

pub(crate) struct SenderUserMessagesSection;

impl SectionContributor for SenderUserMessagesSection {
    fn scope(&self) -> SectionScope {
        SectionScope::Shared
    }

    fn contribute(&self, input: &SectionInput<'_>) -> Result<Option<ContextSection>, SectionError> {
        let Some(context) = input.history.retained_context() else {
            return Ok(None);
        };
        let Some(snapshot) = context.sender_user_messages() else {
            return Ok(None);
        };
        // Preserve historical sender restrictions verbatim, even after a human
        // replies locally. A later reply does not implicitly revoke them.
        let mut items = vec![snapshot.text.clone()];
        let originals = context.local_user_messages_after_sender().map(|(order, message)| {
            let identity = serde_json::json!({"turn_id": message.turn_id, "message_id": message.message_id});
            let text = format!("Retained source order: {order}\nOriginal local source: {identity}\n{}",
                GuardianRootMessage::User(message.text.clone()).render());
            if message.complete && text.len() <= codex_protocol::protocol::TruncationPolicy::Tokens(
                crate::retained_instructions::MAX_INSTRUCTION_TOKENS).byte_budget() {
                text
            } else {
                format!("Retained source order: {order}\nHost: An original local user message is unavailable within the evidence budget. Do not infer permission from missing evidence.\n")
            }
        }).collect::<Vec<_>>();
        if !originals.is_empty() {
            items.push(format!(">>> LOCAL USER MESSAGES AFTER DELIVERY START\nHost: The sender snapshot above belongs to an earlier delivery. These are original user messages accepted locally afterward, not quotations supplied by that sender. Keep their original acceptance order and assess grants and restrictions together; their presence does not imply approval.\n{}>>> LOCAL USER MESSAGES AFTER DELIVERY END\n", originals.concat()));
        }
        Ok(Some(ContextSection::SenderUserMessages { items }))
    }
}

#[cfg(test)]
#[path = "sender_user_messages_tests.rs"]
mod tests;
