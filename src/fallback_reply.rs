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
//!
//! # Known limitation: the destination room is "most recent," not "the one that asked"
//!
//! Posting reuses `live_status::target_room`, which resolves to whichever room most
//! recently talked to the bridge. That is a containment guarantee — the text can only reach
//! a room already in conversation with the bridge, same as `check_outbound_gate` — but it
//! is **not** a guarantee of the *right* room: if a second room pings the bridge mid-turn,
//! the recovered answer goes there instead of to the room whose turn it answers. Accepted
//! for the current single-user deployment, and deliberately not fixed with per-turn room
//! tracking. Revisit if the bridge ever serves more than one conversation at a time.

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
///
/// # Never fires on the first tick
///
/// `previous.is_some()` is part of the debounce, not a redundant guard on the inequality
/// below it. The tick loop's `previous` starts as `None` on every bridge (re)start, so
/// without this a restart beside a transcript whose last turn happens to be sitting
/// unreplied would re-post that turn's text — every restart, however old or already-handled
/// that turn was. Only a `Working -> WaitingForUser` transition *observed live* counts.
///
/// The tradeoff is deliberate: a genuine miss in the seconds before a restart is not caught
/// by this backstop. That was never the primary case (the misses this feature exists for
/// happen during a normal running session), and the offline `tools/missed-reply-scan.mjs`
/// still finds those after the fact. Re-posting stale text unprompted is the worse failure,
/// because it is silent, repeatable, and lands in the room.
pub(crate) fn should_post_fallback(
    previous: Option<AgentState>,
    current: AgentState,
    last_reply_age: Option<Duration>,
) -> bool {
    current == AgentState::WaitingForUser
        && previous.is_some()
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

    /// Exercises the function *in isolation*, with a synthetic `Some(duration)` this
    /// module's real caller could not have produced: before the fix in `status.rs`,
    /// `read_status_at` zeroed `last_reply_age` to `None` for every non-`Working`/`Stalled`
    /// state, so a `WaitingForUser` status never carried a reply age at all and this
    /// argument was vacuously `None` in production. That is exactly how the bug hid behind
    /// a green test. Keep this — it still pins the function's own logic — but
    /// [`a_turn_that_replied_then_kept_talking_does_not_trigger`] below is what proves the
    /// real `read_status_at` -> `should_post_fallback` pipeline is correct.
    #[test]
    fn does_not_fire_when_a_reply_landed() {
        assert!(!should_post_fallback(
            Some(AgentState::Working),
            AgentState::WaitingForUser,
            Some(Duration::from_secs(5)),
        ));
    }

    /// The very first tick after the bridge starts must never post.
    ///
    /// The tick loop's `previous` begins as `None` on every (re)start. If the watched
    /// transcript's last turn happens to be sitting unreplied at that moment — already
    /// handled by hand, or simply stale — firing here would re-post that turn's text into
    /// the room on every single restart. Only a transition observed live counts.
    #[test]
    fn does_not_fire_on_the_first_tick_after_a_restart() {
        assert!(!should_post_fallback(
            None,
            AgentState::WaitingForUser,
            None,
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

    // --- Seam tests: read_status_at -> should_post_fallback ---
    //
    // The unit tests above feed `should_post_fallback` arguments by hand. These run the
    // *real* pipeline the tick loop runs — a fixture transcript through
    // `crate::status::read_status_at`, then its `AgentStatus` straight into
    // `should_post_fallback` — because the one bug this feature shipped with lived
    // precisely in the join between the two, where every isolated test on either side
    // still passed.

    const STALL: Duration = Duration::from_secs(300);

    /// Mirrors `status::tests::status_of`: write fixture lines to a temp transcript and run
    /// the real state machine over them, then hand the result to the real trigger — the
    /// exact chain `live_status.rs`'s tick loop performs.
    fn triggers_for(lines: &[&str], now: &str) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut f = File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        f.flush().unwrap();

        let now = crate::status::parse_timestamp(now).expect("fixture timestamp");
        let status = crate::status::read_status_at(&path, STALL, now);
        should_post_fallback(
            Some(AgentState::Working),
            status.state,
            status.last_reply_age,
        )
    }

    // Chronologically ordered, unlike the extraction constants above, because
    // `read_status_at` reads record order and ages as a sequence.
    const SEAM_REPLY_TOOL_USE: &str = r#"{"type":"assistant","timestamp":"2026-08-08T12:00:02.000Z","message":{"content":[{"type":"tool_use","name":"mcp__matrix__reply","input":{"text":"the answer"}}]}}"#;
    const SEAM_REPLY_TOOL_RESULT: &str = r#"{"type":"user","timestamp":"2026-08-08T12:00:03.000Z","toolUseResult":{"stdout":"ok"},"message":{"content":[{"type":"tool_result"}]}}"#;
    const SEAM_TOOL_USE: &str = r#"{"type":"assistant","timestamp":"2026-08-08T12:00:02.000Z","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#;
    const SEAM_TOOL_RESULT: &str = r#"{"type":"user","timestamp":"2026-08-08T12:00:03.000Z","toolUseResult":{"stdout":"ok"},"message":{"content":[{"type":"tool_result"}]}}"#;
    const SEAM_TEXT: &str = r#"{"type":"assistant","timestamp":"2026-08-08T12:00:05.000Z","message":{"content":[{"type":"text","text":"one more thing"}]}}"#;

    /// The regression this whole fix exists for, at the seam that actually failed.
    ///
    /// A turn that *did* call `mcp__matrix__reply`, and then said one more thing before
    /// settling — the single commonest shape of a healthy turn. It lands in
    /// `WaitingForUser` past `TEXT_GRACE`, and the fallback must stay quiet: the room has
    /// already been answered, and posting the trailing text would double-post every turn.
    ///
    /// Before the `status.rs` fix this asserted `false` and got `true`, because
    /// `read_status_at` zeroed `last_reply_age` for `WaitingForUser`, making
    /// `should_post_fallback`'s `last_reply_age.is_none()` clause dead code.
    #[test]
    fn a_turn_that_replied_then_kept_talking_does_not_trigger() {
        assert!(
            !triggers_for(
                &[
                    PROMPT,
                    SEAM_REPLY_TOOL_USE,
                    SEAM_REPLY_TOOL_RESULT,
                    SEAM_TEXT
                ],
                // Past TEXT_GRACE from SEAM_TEXT at 12:00:05 — the turn has settled.
                "2026-08-08T12:00:30.000Z",
            ),
            "the reply tool landed this turn; the fallback must not post on top of it"
        );
    }

    /// The positive control for the test above, through the same real pipeline: an
    /// identical turn shape with the reply tool call removed. This is the genuine miss the
    /// feature exists to catch, and it must still fire — a fix that silenced both would
    /// look just as green.
    #[test]
    fn a_turn_that_never_replied_still_triggers() {
        assert!(
            triggers_for(
                &[PROMPT, SEAM_TOOL_USE, SEAM_TOOL_RESULT, SEAM_TEXT],
                "2026-08-08T12:00:30.000Z",
            ),
            "no reply landed this turn; the fallback is the only thing that will answer"
        );
    }
}
