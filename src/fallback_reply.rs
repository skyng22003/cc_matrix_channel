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
}
