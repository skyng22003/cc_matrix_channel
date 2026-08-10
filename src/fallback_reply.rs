//! Missed-reply fallback: a backstop for turns where the agent finished without ever
//! calling [`crate::status::REPLY_TOOL_NAME`].
//!
//! # Privacy
//!
//! `crate::status` is deliberately free of message text — see its module doc. This module
//! is the one exception: [`extract_last_turn_text`] reads assistant **text** content
//! blocks (never `thinking`, never tool inputs/outputs), and only after
//! [`should_post_fallback`] has already decided, from metadata alone, that a turn ended
//! with no reply. The text extracted here goes straight back out to the room it would
//! have gone to anyway had the reply tool been called — it is never logged or stored.
//! See `docs/superpowers/specs/2026-08-10-missed-reply-fallback-design.md`.

use std::path::Path;
use std::time::Duration;

use crate::status::AgentState;

/// True exactly when a turn has just ended with no [`crate::status::REPLY_TOOL_NAME`]
/// call landing during it.
///
/// Debounced the same way [`crate::live_status`]'s own terminal-state handling is: fires
/// once on the tick that enters `WaitingForUser`, not on every tick it persists — `previous`
/// is the tick loop's own state-before-this-tick, already threaded through for
/// `live_status::decide`'s use.
pub(crate) fn should_post_fallback(
    previous: Option<AgentState>,
    current: AgentState,
    last_reply_age: Option<Duration>,
) -> bool {
    current == AgentState::WaitingForUser
        && previous != Some(AgentState::WaitingForUser)
        && last_reply_age.is_none()
}

/// Recover the current turn's assistant answer for posting as a fallback reply.
///
/// Concatenates every `text` content block written since the most recent human prompt, in
/// order, joined with blank lines — never `thinking` blocks, never tool inputs or outputs.
/// Returns `None` when there is nothing to post: no transcript, or a turn that ended on
/// tool calls with no trailing text.
///
/// Reads the same tail window `status.rs`'s state machine reads (its private
/// `TAIL_BYTES` constant, via the shared `read_tail` helper) — an exceptionally long turn
/// could lose leading text past that window. Known, accepted limitation, same one
/// `AgentStatus::turn_elapsed` already carries.
pub(crate) fn extract_last_turn_text(path: &Path) -> Option<String> {
    let tail = crate::status::read_tail(path)?;
    let mut parts: Vec<String> = Vec::new();

    for line in tail.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        match value.get("type").and_then(|t| t.as_str()) {
            // A genuine human prompt (not a tool_result disguised as "user") starts a new
            // turn — text collected for a prior turn no longer belongs to "the current
            // turn" once a new one starts.
            Some("user") if value.get("toolUseResult").is_none() => {
                parts.clear();
            }
            Some("assistant") => {
                let blocks = value
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array());
                let Some(blocks) = blocks else { continue };
                for b in blocks {
                    if b.get("type").and_then(|t| t.as_str()) != Some("text") {
                        continue;
                    }
                    if let Some(text) = b.get("text").and_then(|t| t.as_str()) {
                        parts.push(text.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_on_entering_waiting_for_user_with_no_reply() {
        assert!(should_post_fallback(
            Some(AgentState::Working),
            AgentState::WaitingForUser,
            None,
        ));
    }

    #[test]
    fn does_not_fire_when_a_reply_landed() {
        assert!(!should_post_fallback(
            Some(AgentState::Working),
            AgentState::WaitingForUser,
            Some(Duration::from_secs(5)),
        ));
    }

    #[test]
    fn does_not_refire_while_still_waiting() {
        // The debounce: the tick loop's `previous` already equals `WaitingForUser` once
        // this has fired for the turn once, same idiom `live_status::decide` uses for its
        // own terminal-state handling.
        assert!(!should_post_fallback(
            Some(AgentState::WaitingForUser),
            AgentState::WaitingForUser,
            None,
        ));
    }

    #[test]
    fn does_not_fire_for_non_waiting_states() {
        assert!(!should_post_fallback(None, AgentState::Working, None));
        assert!(!should_post_fallback(
            Some(AgentState::Working),
            AgentState::Stalled,
            None
        ));
        assert!(!should_post_fallback(
            Some(AgentState::Working),
            AgentState::Dead,
            None
        ));
    }

    use std::fs::File;
    use std::io::Write;

    /// Write fixture lines to a temp file and extract from the resulting transcript.
    fn extract_of(lines: &[&str]) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut f = File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        f.flush().unwrap();
        extract_last_turn_text(&path)
    }

    const PROMPT: &str = r#"{"type":"user","timestamp":"2026-08-08T12:00:00.000Z","message":{"content":[{"type":"text","text":"do the thing"}]}}"#;
    const PROMPT2: &str = r#"{"type":"user","timestamp":"2026-08-08T12:01:00.000Z","message":{"content":[{"type":"text","text":"do another thing"}]}}"#;
    const TEXT1: &str = r#"{"type":"assistant","timestamp":"2026-08-08T12:00:05.000Z","message":{"content":[{"type":"text","text":"first part"}]}}"#;
    const TEXT2: &str = r#"{"type":"assistant","timestamp":"2026-08-08T12:00:09.000Z","message":{"content":[{"type":"text","text":"second part"}]}}"#;
    const THINKING: &str = r#"{"type":"assistant","timestamp":"2026-08-08T12:00:03.000Z","message":{"content":[{"type":"thinking","thinking":"internal reasoning nobody should see","signature":"x"}]}}"#;
    const TOOL_USE: &str = r#"{"type":"assistant","timestamp":"2026-08-08T12:00:07.000Z","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#;
    const TOOL_RESULT: &str = r#"{"type":"user","timestamp":"2026-08-08T12:00:08.000Z","toolUseResult":{"stdout":"ok"},"message":{"content":[{"type":"tool_result"}]}}"#;

    #[test]
    fn single_text_block_is_returned() {
        assert_eq!(extract_of(&[PROMPT, TEXT1]), Some("first part".to_string()));
    }

    #[test]
    fn thinking_blocks_are_excluded() {
        assert_eq!(
            extract_of(&[PROMPT, THINKING, TEXT1]),
            Some("first part".to_string())
        );
    }

    #[test]
    fn multiple_text_blocks_across_tool_calls_are_concatenated_in_order() {
        assert_eq!(
            extract_of(&[PROMPT, TEXT1, TOOL_USE, TOOL_RESULT, TEXT2]),
            Some("first part\n\nsecond part".to_string())
        );
    }

    #[test]
    fn no_text_blocks_returns_none() {
        assert_eq!(extract_of(&[PROMPT, TOOL_USE, TOOL_RESULT]), None);
    }

    #[test]
    fn only_the_current_turn_is_returned() {
        // TEXT1 belongs to the turn PROMPT opened; PROMPT2 starts a new one, and only
        // TEXT2 (after it) should come back — a stale answer from a prior turn must never
        // get relayed as if it were this turn's.
        assert_eq!(
            extract_of(&[PROMPT, TEXT1, PROMPT2, TEXT2]),
            Some("second part".to_string())
        );
    }

    #[test]
    fn missing_transcript_returns_none() {
        assert_eq!(
            extract_last_turn_text(std::path::Path::new("/nonexistent/session.jsonl")),
            None
        );
    }
}
