//! Pre-compaction memory flush for native runs.
//!
//! When an automatic compaction is due and the run may call the bundled
//! `memory_save` tool, the gateway first sends one more round with the full
//! context and an added instruction to save lasting notes, then compacts
//! before the round after it. The instruction is added to the system prompt
//! of that round only; it never replaces the user's turn, and the model
//! still answers it. Saving goes through the ordinary tool path: the user's
//! grant, the conversation's permission (including approval) and the
//! extension's own path checks all apply, so a conversation that may not
//! save notes gets no flush.
//!
//! The flush is offered once for each summary (recorded as
//! [`MemoryFlush`] on the thread, so a restart or a later run does not
//! offer it again before the next compaction). Manual compaction has no
//! flush: no run is in progress to save anything.
//!
//! Follows RustyClaw's `memory_flush.rs` (an inline instruction that keeps
//! the user's turn), but triggers at the compaction threshold rather than a
//! fixed token count before it, and names the scoped tool instead of
//! generic file tools.
use crate::extension_tools::tool_name;
use a206_core::*;

/// The bundled memory extension and its save tool.
const EXTENSION: &str = "agent206.memory";
const SAVE: &str = "memory_save";

/// The save tool's name as runs are offered it.
pub(super) fn save_tool() -> String {
    tool_name(EXTENSION, SAVE)
}

/// Whether a flush is still to be offered before the next summary.
pub(super) fn due(thread: &Thread) -> bool {
    thread.memory_flush
        != Some(MemoryFlush {
            after: thread.latest_compaction(),
        })
}

/// Record that the flush for the current summary was offered.
pub(super) fn mark(thread: &mut Thread) {
    thread.memory_flush = Some(MemoryFlush {
        after: thread.latest_compaction(),
    });
}

/// The system-prompt section for the flush round. `today` is the
/// gateway's local date (`YYYY-MM-DD`), which names the suggested note.
pub(super) fn instruction(today: &str) -> String {
    let tool = save_tool();
    format!(
        "\n\n# Pre-compaction memory flush\n\nThis conversation is close to its context limit, so older messages will be summarized after this response. Before continuing, use the `{tool}` tool to save anything worth remembering beyond this conversation that is not already saved: the user's goals and preferences, decisions and their reasons, and facts about the project. Use `memory/{today}.md` unless an existing note fits better; saving appends, so never repeat what a note already holds. Then continue with the user's request as usual. If nothing needs saving, just continue."
    )
}

/// The gateway's local date for [`instruction`].
pub(super) fn today() -> String {
    jiff::Zoned::now().date().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> Message {
        let mut message = Message::new(
            Role::System,
            Id::new_v4().to_string(),
            0,
            CodingBackend::Native,
        );
        message.compaction = Some(Compaction {
            trigger: CompactionTrigger::Automatic,
            messages: 1,
            previous: None,
            input_tokens: None,
            context_limit: None,
            usage: Usage {
                input_tokens: None,
                output_tokens: None,
                cost: None,
            },
            request: None,
        });
        message
    }

    #[test]
    fn a_flush_is_offered_once_for_each_summary() {
        let mut thread = Thread::new(Id::new_v4().to_string(), Mode::default());
        assert!(due(&thread));
        mark(&mut thread);
        assert!(!due(&thread));
        // A new summary starts a new cycle.
        thread.messages.push(summary());
        assert!(due(&thread));
        mark(&mut thread);
        assert!(!due(&thread));
        // Messages after the summary do not.
        thread.messages.push(Message::new(
            Role::User,
            Id::new_v4().to_string(),
            0,
            CodingBackend::Native,
        ));
        assert!(!due(&thread));
    }

    #[test]
    fn the_instruction_names_the_save_tool_and_the_date() {
        let today = Id::new_v4().to_string();
        let text = instruction(&today);
        assert!(text.contains(&format!("`{}`", save_tool())), "{text}");
        assert!(text.contains(&format!("memory/{today}.md")), "{text}");
    }
}
