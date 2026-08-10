//! Live agent-status message: the draft/edit pattern.
//!
//! One message per working spell, edited in place as the agent works, following the
//! OpenClaw/Hermes "draft that keeps updating" behaviour. The terminal state is edited
//! into that same message, so a normal turn leaves exactly one status message behind
//! rather than a running commentary.
//!
//! Matrix edits do not raise push notifications, which is the right default here: when the
//! agent finishes it sends its actual reply, and that reply is its own message and pings
//! the user by itself — a separate "done" would double-ping every single turn.
//!
//! The exception is [`AgentState::needs_alert`] (stalled, dead). Nothing else will ever
//! arrive in those cases, so they additionally get a short **new** message. Otherwise the
//! user's device stays silent exactly when the agent is wedged and they have walked away.
//!
//! Read [`crate::status`] for how the state itself is derived. This module only decides
//! what to put on the wire and when.
//!
//! # The draft delay is a boundary, not a guarantee
//!
//! Any fixed delay has turns that finish just past it — the delay predicts the rest of a
//! turn from elapsed time alone, and turns end for reasons that have nothing to do with
//! how long they have already run. Two things narrow that residual without chasing it to
//! zero, which would mean never drafting mid-turn at all:
//!
//! - [`reply_anchored`] re-times the clock from the room's own reply when there is one,
//!   so bookkeeping after the reply doesn't read as "still running" to the delay.
//! - The `StartDraft` arm holds the very first send of a spell for one extra
//!   [`TICK`], sending only if the spell is still `Working` then. A turn that stops inside
//!   that window never gets a message; one still running pays [`TICK`] of latency for it.
//!
//! Neither depends on the other, and neither needs a `Stop` hook: firing a hook is not
//! itself observable in the transcript this module already reads (checked directly against
//! a real `Stop` hook — no `attachment` record is written the way `SessionStart`'s is), so
//! reaching the bridge from one would need new IPC. Both mechanisms below work with what
//! `crate::status` can already see.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use matrix_sdk::Client;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
use tokio_util::sync::CancellationToken;

use crate::status::{AgentState, AgentStatus, TEXT_GRACE, read_status, stall_threshold};

use crate::access::{AccessControl, ChunkMode};
use crate::fallback_reply::{extract_last_turn_text, should_post_fallback};
use crate::mcp::chunk_message;

/// Poll cadence. Also the floor on edit frequency: every edit is a `room.send`, and
/// homeservers rate-limit sends, so this must stay comfortably above one-per-second.
const TICK: Duration = Duration::from_secs(3);

/// Matches the cap `mcp::edit_message` enforces on message bodies.
const MAX_TOTAL_LENGTH: usize = 50_000;

/// How long the agent must be working before a status message is worth posting at all.
///
/// Most turns finish quickly, and for those the agent's own reply is the only message the
/// room needs — a status draft for a six-second turn is pure clutter. Only once a turn
/// runs long enough that the user might reasonably wonder what is happening does the draft
/// appear. Tunable via `CC_MATRIX_DRAFT_DELAY_SECS`.
const DEFAULT_DRAFT_DELAY_SECS: u64 = 20;

fn draft_delay() -> Duration {
    let secs = std::env::var("CC_MATRIX_DRAFT_DELAY_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DRAFT_DELAY_SECS);
    Duration::from_secs(secs)
}

/// Tracks the draft message for the current working spell.
struct Draft {
    room_id: OwnedRoomId,
    event_id: OwnedEventId,
    /// Last body actually sent, so an unchanged render does not burn homeserver quota.
    rendered: String,
}

/// The decision the state machine makes on each tick, kept separate from the Matrix calls
/// so it can be unit-tested without a homeserver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Nothing changed worth spending a send on.
    Nothing,
    /// Crossed the draft delay for the first time this spell. Don't send yet — see
    /// [`decide`]'s `held_draft` parameter. The caller remembers this happened and asks
    /// again next tick; sending only follows from a *second* consecutive crossing.
    HoldDraft,
    /// Start a new working spell: send a message and remember its event id. Only reached
    /// once a spell has crossed the delay on two consecutive ticks — see [`HoldDraft`](Action::HoldDraft).
    StartDraft,
    /// Update the existing draft in place.
    EditDraft,
    /// End the spell: edit the draft to its final state and close it. A separate alert
    /// message follows only for states that would otherwise be silent — see
    /// [`AgentState::needs_alert`].
    CloseDraft,
    /// Something went wrong before any draft existed — a stall or death inside the
    /// draft delay. Send the alert on its own, because a silent failure with no message
    /// at all is the one outcome this whole feature exists to prevent.
    AlertOnly,
}

/// Where a working spell's draft message currently stands.
///
/// Replaces what would otherwise be two separate booleans (`has_draft`, `held_draft`) on
/// [`decide`] — besides keeping the argument count down, a plain pair of bools would admit
/// a state, "has_draft and held_draft both true," that can never actually happen (a spell
/// stops being held the moment it actually sends). The enum makes that state
/// unrepresentable instead of just unreached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DraftState {
    /// No draft yet, and the delay hasn't been crossed — or it has, but this is the first
    /// crossing and it's being held one tick; see [`Action::HoldDraft`].
    None,
    /// The delay was crossed on a previous tick and held. If still `Working` past the
    /// delay this tick too, the hold is confirmed and it sends.
    Held,
    /// A draft message is open in the room.
    Open,
}

/// Re-anchor the spell clock on the room's own reply, when there is one.
///
/// `working_for` measures from the human's prompt — the whole turn, including whatever
/// bookkeeping happens after the room has already been answered. Once
/// [`crate::status::REPLY_TOOL_NAME`] has landed, the room isn't waiting on that clock
/// any more; it's waiting on however long the agent keeps going *after* its own reply.
/// `last_reply_age` is always `<=` `working_for` when the reply happened during the
/// current spell — it can't predate the spell's own start — so this only ever pulls the
/// clock forward, never back past a reply that hasn't happened yet.
fn reply_anchored(
    working_for: Option<Duration>,
    last_reply_age: Option<Duration>,
) -> Option<Duration> {
    match (working_for, last_reply_age) {
        (Some(w), Some(r)) if r < w => Some(r),
        (w, _) => w,
    }
}

/// Decide what to do this tick.
///
/// `previous` is the state observed last tick; `draft` is where the current spell's draft
/// message stands — see [`DraftState`]; `changed` is whether the rendered body differs
/// from what was last sent; `working_for` is how long the agent has been continuously
/// working, or `None` if it is not working — already passed through [`reply_anchored`] by
/// the caller, so this is "how long since the room last had reason to wait," not
/// necessarily the raw turn duration; `grace_held` is [`AgentStatus::grace_held`] — whether
/// the current `Working` rests only on a reply that is still inside the text grace window.
fn decide(
    previous: Option<AgentState>,
    current: AgentState,
    draft: DraftState,
    changed: bool,
    working_for: Option<Duration>,
    grace_held: bool,
    draft_delay: Duration,
) -> Action {
    match current {
        // "No information" must never be broadcast as if it were a result.
        AgentState::Unknown => Action::Nothing,

        AgentState::Working => match draft {
            DraftState::Open => {
                // Suppress no-op edits: re-rendering an identical body every tick would
                // spend homeserver quota for nothing.
                if changed {
                    Action::EditDraft
                } else {
                    Action::Nothing
                }
            }
            DraftState::None | DraftState::Held => {
                // The spell clock counts the text grace window too, so a spell can cross
                // the delay while the only evidence of work is a reply already sent — a
                // turn of half the delay reaches it from the far side of its own reply.
                // Discount that window rather than trusting the raw spell, so the delay
                // means "the turn itself has run this long" either way. A spell can only
                // outrun its turn by the grace, so once it is a whole grace past the
                // delay the turn really has run the full delay, whatever the last record
                // happens to be.
                let required = if grace_held {
                    draft_delay + TEXT_GRACE
                } else {
                    draft_delay
                };
                if working_for.is_some_and(|d| d >= required) {
                    // Deferred send: the first crossing only marks intent. A spell that
                    // stops within this one extra tick never gets a message at all — the
                    // near-miss costs nothing, because nothing went out. Any threshold
                    // still has turns ending just past it (raising the delay only moves
                    // the boundary), so this doesn't chase that to zero — it only removes
                    // the ones inside one tick of it, which is where near-misses cluster.
                    if draft == DraftState::Held {
                        Action::StartDraft
                    } else {
                        Action::HoldDraft
                    }
                } else {
                    Action::Nothing
                }
            }
        },

        // Debounce: act once on entering the state, not every tick it persists.
        s if s.is_terminal() => {
            if previous == Some(s) {
                Action::Nothing
            } else if draft == DraftState::Open {
                // Only close a spell we actually opened — otherwise a bridge starting up
                // beside an idle agent would declare "waiting for you" unprompted. A held
                // (not yet sent) draft has nothing to close either, same as no draft at
                // all — see `a_held_draft_that_stops_before_confirming_sends_nothing`.
                Action::CloseDraft
            } else if s.needs_alert() {
                // No draft, because the turn was short — but a short turn that ends in a
                // stall or a dead process still has to be reported.
                Action::AlertOnly
            } else {
                // A quick turn that simply finished. The agent's own reply is the message.
                Action::Nothing
            }
        }

        _ => Action::Nothing,
    }
}

fn render_working(status: &AgentStatus) -> String {
    let mut body = format!("⏳ **Claude is {}**", status.state);
    let detail = status.render();
    // Drop the leading "Agent:" line — the heading above already says it.
    if let Some(rest) = detail.split_once('\n').map(|(_, r)| r) {
        body.push('\n');
        body.push_str(rest);
    }
    truncate(body)
}

fn render_terminal(status: &AgentStatus) -> String {
    let icon = match status.state {
        AgentState::Stalled => "⚠️",
        AgentState::Dead => "❌",
        _ => "✅",
    };
    let mut body = format!("{icon} **Claude is {}**", status.state);
    if let Some(elapsed) = status.turn_elapsed {
        body.push_str(&format!(
            "\nTurn ran for {}",
            crate::status::format_duration(elapsed)
        ));
    }
    if let Some(age) = status.last_activity_age
        && matches!(status.state, AgentState::Stalled)
    {
        body.push_str(&format!(
            "\nNo activity for {}",
            crate::status::format_duration(age)
        ));
    }
    truncate(body)
}

/// Short alert body, sent as a *new* message so it actually push-notifies.
///
/// Deliberately terse: the edited draft above it already carries the detail, and this
/// exists to make a phone buzz, not to be read in full on a lock screen.
fn render_alert(status: &AgentStatus) -> String {
    let body = match status.state {
        AgentState::Dead => "❌ **Claude is not running** — the session ended".to_string(),
        _ => match status.last_activity_age {
            Some(age) => format!(
                "⚠️ **Claude looks stuck** — no activity for {}",
                crate::status::format_duration(age)
            ),
            None => "⚠️ **Claude looks stuck**".to_string(),
        },
    };
    truncate(body)
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_TOTAL_LENGTH {
        s.truncate(MAX_TOTAL_LENGTH);
    }
    s
}

/// Pick the room to post status into: the one that most recently talked to the bridge.
///
/// Falls back to any known room only when there is no recorded last-active room — and
/// that fallback is genuinely arbitrary, because `known_rooms` is a `HashSet` with no
/// ordering. `last_active_room` exists precisely so the normal path is deterministic.
///
/// Both are populated by inbound traffic, so status can only ever go to a room that has
/// already talked to the bridge — the same containment property `check_outbound_gate`
/// gives the MCP tools.
fn target_room(
    known_rooms: &Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>>,
    last_active_room: &Arc<parking_lot::Mutex<Option<OwnedRoomId>>>,
) -> Option<OwnedRoomId> {
    if let Some(room) = last_active_room.lock().clone() {
        return Some(room);
    }
    known_rooms.lock().iter().next().cloned()
}

/// Post a new status message and return its event id.
///
/// Split out of the loop so the Matrix write path can be exercised against a real
/// homeserver by `live_draft_cycle_against_real_homeserver` below.
async fn send_status(client: &Client, room_id: &OwnedRoomId, body: &str) -> Option<OwnedEventId> {
    let room = client.get_room(room_id)?;
    match room
        .send(RoomMessageEventContent::text_markdown(body))
        .await
    {
        Ok(resp) => Some(resp.event_id),
        Err(e) => {
            tracing::warn!("Failed to send status message: {e}");
            None
        }
    }
}

/// Edit an existing status message in place. Returns whether the edit landed.
async fn edit_status(
    client: &Client,
    room_id: &OwnedRoomId,
    event_id: &OwnedEventId,
    body: &str,
) -> bool {
    let Some(room) = client.get_room(room_id) else {
        return false;
    };
    let content = RoomMessageEventContent::text_markdown(body);
    let edited = match room
        .make_edit_event(
            event_id,
            matrix_sdk::room::edit::EditedContent::RoomMessage(content.into()),
        )
        .await
    {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("Failed to build status edit: {e}");
            return false;
        }
    };
    match room.send(edited).await {
        Ok(_) => true,
        Err(e) => {
            // Caller keeps the draft open and retries next tick — a transient send
            // failure should not orphan the message.
            tracing::warn!("Failed to send status edit: {e}");
            false
        }
    }
}

/// Post recovered turn text as a fallback reply, chunked the same way `mcp::reply` chunks
/// an explicit one — a long recovered answer should be split, not silently cut off the
/// way status messages are by [`truncate`]. Returns how many chunks actually sent, for
/// logging (and for the integration test below to assert against).
async fn post_fallback_chunks(
    client: &Client,
    room_id: &OwnedRoomId,
    text: &str,
    chunk_limit: usize,
    chunk_mode: &ChunkMode,
) -> usize {
    let Some(room) = client.get_room(room_id) else {
        return 0;
    };
    let mut sent = 0;
    for chunk in chunk_message(text, chunk_limit, chunk_mode) {
        match room
            .send(RoomMessageEventContent::text_markdown(chunk))
            .await
        {
            Ok(_) => sent += 1,
            Err(e) => tracing::warn!("Failed to send fallback reply chunk: {e}"),
        }
    }
    sent
}

/// Spawn the live-status loop. Returns immediately.
pub fn spawn(
    client: Arc<Client>,
    known_rooms: Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>>,
    last_active_room: Arc<parking_lot::Mutex<Option<OwnedRoomId>>>,
    access_control: Arc<AccessControl>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let threshold = stall_threshold();
        let delay = draft_delay();

        match crate::status::transcript_path_with_source() {
            Some((path, source)) => tracing::info!(
                transcript = %path.display(),
                %source,
                draft_delay_secs = delay.as_secs(),
                stall_threshold_secs = threshold.as_secs(),
                tick_secs = TICK.as_secs(),
                "Live status loop starting"
            ),
            None => tracing::info!(
                transcript = "none",
                draft_delay_secs = delay.as_secs(),
                stall_threshold_secs = threshold.as_secs(),
                "Live status loop starting with no transcript resolved"
            ),
        }

        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut previous: Option<AgentState> = None;
        let mut draft: Option<Draft> = None;
        // Which transcript the last tick read. Logged on change: a mid-run switch means the
        // fallback was in play, and a spell measured against another session's activity is
        // not this session's spell at all.
        let mut last_transcript: Option<(std::path::PathBuf, crate::status::TranscriptSource)> =
            None;
        // Measured locally rather than from `turn_elapsed`, which is None once the opening
        // prompt scrolls out of the transcript tail — exactly on the long turns that need
        // a draft most.
        let mut working_since: Option<std::time::Instant> = None;
        // True after a tick decides StartDraft but chooses to hold rather than send —
        // see the comment at the `StartDraft` arm below. Reset whenever the spell resets,
        // same as `working_since`: a held decision belongs to the spell that made it.
        let mut held_draft: bool = false;

        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = cancel.cancelled() => break,
            }

            let status = read_status(threshold);

            // Report a transcript switch before anything else: it reframes every number
            // logged below it.
            let resolved = crate::status::transcript_path_with_source();
            if resolved != last_transcript {
                match (&resolved, &last_transcript) {
                    (Some((path, source)), None) => tracing::info!(
                        transcript = %path.display(),
                        %source,
                        "Status transcript resolved"
                    ),
                    (Some((path, source)), Some((old_path, old_source))) => tracing::info!(
                        transcript = %path.display(),
                        %source,
                        previous_transcript = %old_path.display(),
                        previous_source = %old_source,
                        "Status transcript changed"
                    ),
                    (None, _) => tracing::info!("Status transcript no longer resolvable"),
                }
                last_transcript = resolved.clone();
            }

            if matches!(status.state, AgentState::Working) {
                if working_since.is_none() {
                    tracing::info!(
                        state = %status.state,
                        last_activity_age_secs = status.last_activity_age.map(|d| d.as_secs()),
                        turn_elapsed_secs = status.turn_elapsed.map(|d| d.as_secs()),
                        "Working spell started (draft clock anchored here)"
                    );
                }
                working_since.get_or_insert_with(std::time::Instant::now);
            } else {
                if working_since.is_some() {
                    tracing::info!(
                        state = %status.state,
                        spell_age_secs = working_since.map(|t| t.elapsed().as_secs()),
                        "Working spell ended (draft clock reset)"
                    );
                }
                working_since = None;
                held_draft = false;
            }
            let working_for = working_since.map(|t| t.elapsed());
            // The clock `decide` actually gates on: pulled forward to the room's own reply
            // when there is one, so bookkeeping after the reply doesn't keep the turn
            // "running" as far as the draft delay is concerned. See `reply_anchored`.
            let anchored_for = reply_anchored(working_for, status.last_reply_age);
            let body = if matches!(status.state, AgentState::Working) {
                render_working(&status)
            } else {
                render_terminal(&status)
            };
            let changed = draft.as_ref().is_none_or(|d| d.rendered != body);
            let draft_state = if draft.is_some() {
                DraftState::Open
            } else if held_draft {
                DraftState::Held
            } else {
                DraftState::None
            };

            let action = decide(
                previous,
                status.state,
                draft_state,
                changed,
                anchored_for,
                status.grace_held,
                delay,
            );

            // Every tick at debug; anything that costs a send at info. Metadata only — the
            // rendered body is never logged, per the privacy note in `crate::status`.
            if action == Action::Nothing {
                tracing::debug!(
                    ?action,
                    state = %status.state,
                    previous = ?previous,
                    has_draft = draft.is_some(),
                    changed,
                    spell_age_secs = working_for.map(|d| d.as_secs()),
                    anchored_age_secs = anchored_for.map(|d| d.as_secs()),
                    "Status tick"
                );
            } else {
                tracing::info!(
                    ?action,
                    state = %status.state,
                    previous = ?previous,
                    has_draft = draft.is_some(),
                    changed,
                    spell_age_secs = working_for.map(|d| d.as_secs()),
                    anchored_age_secs = anchored_for.map(|d| d.as_secs()),
                    draft_delay_secs = delay.as_secs(),
                    "Status decision"
                );
            }

            match action {
                Action::Nothing => {}

                Action::HoldDraft => {
                    held_draft = true;
                    tracing::info!(
                        spell_age_secs = working_for.map(|d| d.as_secs()),
                        anchored_age_secs = anchored_for.map(|d| d.as_secs()),
                        "Status draft held one tick before sending"
                    );
                }

                Action::StartDraft => {
                    held_draft = false;

                    let Some(room_id) = target_room(&known_rooms, &last_active_room) else {
                        // No room has talked to us yet; nothing to update.
                        tracing::info!("Status draft skipped: no target room yet");
                        previous = Some(status.state);
                        continue;
                    };
                    if let Some(event_id) = send_status(&client, &room_id, &body).await {
                        tracing::info!(
                            room_id = %room_id,
                            event_id = %event_id,
                            spell_age_secs = working_for.map(|d| d.as_secs()),
                            "Status draft posted"
                        );
                        draft = Some(Draft {
                            room_id,
                            event_id,
                            rendered: body,
                        });
                    }
                }

                Action::EditDraft => {
                    if let Some(d) = draft.as_mut()
                        && edit_status(&client, &d.room_id, &d.event_id, &body).await
                    {
                        tracing::info!(
                            room_id = %d.room_id,
                            event_id = %d.event_id,
                            spell_age_secs = working_for.map(|d| d.as_secs()),
                            "Status draft edited"
                        );
                        d.rendered = body;
                    }
                }

                Action::CloseDraft => {
                    if let Some(d) = draft.take() {
                        // Fold the final state into the draft, so a normal turn leaves one
                        // status message rather than a running commentary.
                        edit_status(&client, &d.room_id, &d.event_id, &body).await;
                        tracing::info!(
                            room_id = %d.room_id,
                            event_id = %d.event_id,
                            state = %status.state,
                            alerting = status.state.needs_alert(),
                            "Status draft closed"
                        );

                        // That edit is silent. For states where nothing else will ever
                        // arrive, follow it with a short new message that actually pings.
                        if status.state.needs_alert() {
                            send_status(&client, &d.room_id, &render_alert(&status)).await;
                        }
                    }
                }

                Action::AlertOnly => {
                    if let Some(room_id) = target_room(&known_rooms, &last_active_room) {
                        tracing::info!(
                            room_id = %room_id,
                            state = %status.state,
                            "Status alert sent with no draft"
                        );
                        send_status(&client, &room_id, &render_alert(&status)).await;
                    }
                }
            }

            if should_post_fallback(previous, status.state, status.last_reply_age) {
                match resolved
                    .as_ref()
                    .and_then(|(path, _)| extract_last_turn_text(path))
                {
                    Some(text) => match target_room(&known_rooms, &last_active_room) {
                        Some(room_id) => {
                            let chunk_limit = access_control.text_chunk_limit();
                            let chunk_mode = access_control.chunk_mode();
                            let sent = post_fallback_chunks(
                                &client,
                                &room_id,
                                &text,
                                chunk_limit,
                                &chunk_mode,
                            )
                            .await;
                            tracing::info!(
                                room_id = %room_id,
                                chunks_sent = sent,
                                "Missed-reply fallback: reply tool was never called this turn, posted recovered text"
                            );
                        }
                        None => {
                            tracing::info!("Missed-reply fallback triggered but no target room yet")
                        }
                    },
                    None => tracing::info!("Missed-reply fallback triggered but no text recovered"),
                }
            }

            previous = Some(status.state);
        }

        tracing::debug!("Live status loop stopped");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELAY: Duration = Duration::from_secs(20);
    /// Comfortably past `DELAY` — "this turn has been running a while".
    const LONG: Duration = Duration::from_secs(60);
    /// Comfortably inside `DELAY` — "this turn just started".
    const SHORT: Duration = Duration::from_secs(2);
    /// Past `DELAY`, but by less than the grace window. A spell can reach here purely by
    /// the grace tacked onto a turn that finished well short of the delay.
    const JUST_PAST_DELAY: Duration = Duration::from_secs(21);

    fn status(state: AgentState) -> AgentStatus {
        AgentStatus {
            state,
            last_activity_age: Some(Duration::from_secs(4)),
            last_tool: Some("Bash".to_string()),
            turn_elapsed: Some(Duration::from_secs(200)),
            grace_held: false,
            last_reply_age: None,
        }
    }

    /// The first tick to cross the delay does not send — see [`DraftState::Held`] on
    /// [`decide`] and the deferred-send tests below. It marks intent and waits one more
    /// tick.
    #[test]
    fn first_working_tick_holds_rather_than_sends() {
        assert_eq!(
            decide(
                None,
                AgentState::Working,
                DraftState::None,
                true,
                Some(LONG),
                false,
                DELAY
            ),
            Action::HoldDraft
        );
    }

    /// A second consecutive crossing — `DraftState::Held`, meaning the previous tick
    /// already returned `HoldDraft` for this spell — is what actually sends.
    #[test]
    fn second_consecutive_crossing_sends() {
        assert_eq!(
            decide(
                None,
                AgentState::Working,
                DraftState::Held,
                true,
                Some(LONG),
                false,
                DELAY
            ),
            Action::StartDraft
        );
    }

    /// A held draft that never gets confirmed — the spell ends before the next tick — must
    /// not send anything. No draft was ever opened (`DraftState::Held`, not `Open`, because
    /// holding never sends), so the terminal branch below has nothing to close; this is the
    /// whole point of deferring the send. The hold itself needs no special-casing to
    /// cancel — it just never gets asked again.
    #[test]
    fn a_held_draft_that_stops_before_confirming_sends_nothing() {
        assert_eq!(
            decide(
                Some(AgentState::Working),
                AgentState::WaitingForUser,
                DraftState::Held,
                true,
                None,
                false,
                DELAY
            ),
            Action::Nothing
        );
    }

    #[test]
    fn subsequent_working_ticks_edit_rather_than_send() {
        assert_eq!(
            decide(
                Some(AgentState::Working),
                AgentState::Working,
                DraftState::Open,
                true,
                Some(LONG),
                false,
                DELAY
            ),
            Action::EditDraft
        );
    }

    /// No-op suppression: an unchanged render must not cost a send.
    #[test]
    fn unchanged_render_sends_nothing() {
        assert_eq!(
            decide(
                Some(AgentState::Working),
                AgentState::Working,
                DraftState::Open,
                false,
                Some(LONG),
                false,
                DELAY
            ),
            Action::Nothing
        );
    }

    /// The point of the draft delay: a turn that finishes quickly must leave the room
    /// exactly as it found it. The agent's own reply is the only message such a turn needs.
    #[test]
    fn a_quick_turn_posts_nothing_at_all() {
        // Working, but not for long enough to be worth mentioning.
        assert_eq!(
            decide(
                None,
                AgentState::Working,
                DraftState::None,
                true,
                Some(SHORT),
                false,
                DELAY
            ),
            Action::Nothing
        );
        // ...and then it finishes. Still nothing: no draft was ever opened.
        assert_eq!(
            decide(
                Some(AgentState::Working),
                AgentState::WaitingForUser,
                DraftState::None,
                true,
                None,
                false,
                DELAY
            ),
            Action::Nothing
        );
    }

    /// The spell clock keeps running through the text grace window, so a turn barely over
    /// half the delay still reaches the delay — from the far side of its own reply.
    /// Opening a draft there posts a status for a turn that is already finished, which is
    /// precisely the clutter the delay exists to prevent.
    #[test]
    fn no_draft_opens_on_a_turn_that_may_already_be_over() {
        assert_eq!(
            decide(
                Some(AgentState::Working),
                AgentState::Working,
                DraftState::None,
                true,
                Some(JUST_PAST_DELAY),
                true,
                DELAY
            ),
            Action::Nothing
        );
    }

    /// ...but waiting does tell the two apart. A spell can only outrun the turn by the
    /// grace window, so once it is a whole grace window past the delay the turn itself has
    /// genuinely run the full delay, whatever the last record happens to be. A turn that
    /// never calls a tool still deserves its draft. `DraftState::Held` isolates the
    /// threshold computation from the separate hold-one-tick mechanic tested above.
    #[test]
    fn a_long_turn_drafts_even_if_its_last_record_is_text() {
        assert_eq!(
            decide(
                Some(AgentState::Working),
                AgentState::Working,
                DraftState::Held,
                true,
                Some(LONG),
                true,
                DELAY
            ),
            Action::StartDraft
        );
    }

    #[test]
    fn draft_opens_once_the_turn_runs_long() {
        assert_eq!(
            decide(
                None,
                AgentState::Working,
                DraftState::Held,
                true,
                Some(LONG),
                false,
                DELAY
            ),
            Action::StartDraft
        );
    }

    /// A short turn that stalls or dies still has to be reported, even though the draft
    /// delay meant no draft existed to fold the news into. Silence here is the one
    /// outcome the whole feature exists to prevent.
    #[test]
    fn short_turn_that_fails_still_alerts() {
        for bad in [AgentState::Stalled, AgentState::Dead] {
            assert_eq!(
                decide(
                    Some(AgentState::Working),
                    bad,
                    DraftState::None,
                    true,
                    None,
                    false,
                    DELAY
                ),
                Action::AlertOnly,
                "{bad:?} must be reported even with no draft open"
            );
        }
    }

    /// Every terminal state folds back into the draft rather than adding a message.
    #[test]
    fn terminal_transition_closes_the_draft() {
        for terminal in [
            AgentState::Stalled,
            AgentState::WaitingForUser,
            AgentState::Dead,
        ] {
            assert_eq!(
                decide(
                    Some(AgentState::Working),
                    terminal,
                    DraftState::Open,
                    true,
                    None,
                    false,
                    DELAY
                ),
                Action::CloseDraft,
                "{terminal:?} should close the draft in place"
            );
        }
    }

    /// The push-notification rule, which is what decides whether an extra message is sent.
    ///
    /// Finishing normally must NOT alert: the agent's own reply is a separate message and
    /// already pings, so alerting here would double-ping every turn — the redundancy that
    /// prompted this design. Stalled and dead must alert, because a silent edit is the
    /// only thing that would ever arrive.
    #[test]
    fn only_silent_failures_raise_an_alert() {
        assert!(!AgentState::WaitingForUser.needs_alert());
        assert!(AgentState::Stalled.needs_alert());
        assert!(AgentState::Dead.needs_alert());
        assert!(!AgentState::Working.needs_alert());
        assert!(!AgentState::Unknown.needs_alert());
    }

    /// The alert is a standalone message, so it must stand alone — the detail lives in
    /// the edited draft above it, but this line has to say what happened by itself.
    #[test]
    fn alert_body_is_self_contained() {
        let stalled = render_alert(&status(AgentState::Stalled));
        assert!(stalled.contains("stuck"));
        assert!(stalled.contains("4s"), "should carry the age: {stalled}");

        let dead = render_alert(&status(AgentState::Dead));
        assert!(dead.contains("not running"));

        // Metadata only, same rule as everywhere else in this module.
        for body in [stalled, dead] {
            assert!(!body.contains("Bash"));
        }
    }

    /// Debounce: closing fires once on entry, not on every tick that follows.
    #[test]
    fn terminal_state_is_announced_once() {
        // Enters Stalled: close the draft, which consumes it.
        assert_eq!(
            decide(
                Some(AgentState::Working),
                AgentState::Stalled,
                DraftState::Open,
                true,
                None,
                false,
                DELAY
            ),
            Action::CloseDraft
        );
        // Still stalled next tick, draft now closed: silence.
        assert_eq!(
            decide(
                Some(AgentState::Stalled),
                AgentState::Stalled,
                DraftState::None,
                true,
                None,
                false,
                DELAY
            ),
            Action::Nothing
        );
    }

    /// stall → recover → stall must produce exactly one announcement per entry. Also
    /// exercises `DraftState` transitions across ticks the way the real loop does: a
    /// `HoldDraft` tick must not itself count as an announcement, only the confirmed
    /// `StartDraft` after it.
    #[test]
    fn stall_recover_stall_fires_once_per_transition() {
        let mut draft_state = DraftState::None;
        let mut previous: Option<AgentState> = None;
        let mut terminals = 0;
        let mut starts = 0;
        let mut holds = 0;

        // Each recovery gets two Working ticks, not one: sending now costs a hold tick
        // before the confirming send, so a spell needs to survive at least that long to
        // ever reach `StartDraft` at all. A single-tick recovery is exercised separately
        // in `a_held_draft_that_stops_before_confirming_sends_nothing` — it never sends,
        // by design, and still alerts independently when it stalls.
        let sequence = [
            AgentState::Working,
            AgentState::Working,
            AgentState::Stalled,
            AgentState::Stalled,
            AgentState::Working,
            AgentState::Working,
            AgentState::Stalled,
            AgentState::Stalled,
        ];
        for state in sequence {
            match decide(previous, state, draft_state, true, Some(LONG), false, DELAY) {
                Action::HoldDraft => {
                    draft_state = DraftState::Held;
                    holds += 1;
                }
                Action::StartDraft => {
                    draft_state = DraftState::Open;
                    starts += 1;
                }
                Action::CloseDraft | Action::AlertOnly => {
                    draft_state = DraftState::None;
                    terminals += 1;
                }
                Action::EditDraft | Action::Nothing => {}
            }
            previous = Some(state);
        }

        assert_eq!(
            holds, 2,
            "one hold tick per working spell, ahead of its send"
        );
        assert_eq!(starts, 2, "one draft per working spell");
        assert_eq!(terminals, 2, "one announcement per stall entry");
    }

    /// A bridge that starts up while the agent is idle must not volunteer a status
    /// message into the room unprompted.
    #[test]
    fn no_announcement_without_a_preceding_working_spell() {
        assert_eq!(
            decide(
                None,
                AgentState::WaitingForUser,
                DraftState::None,
                true,
                None,
                false,
                DELAY
            ),
            Action::Nothing
        );
    }

    #[test]
    fn unknown_state_is_never_broadcast() {
        assert_eq!(
            decide(
                Some(AgentState::Working),
                AgentState::Unknown,
                DraftState::Open,
                true,
                Some(LONG),
                false,
                DELAY
            ),
            Action::Nothing
        );
    }

    // --- reply_anchored ---

    /// The whole point: once a reply has landed, the clock the delay gates on is measured
    /// from the reply, not the prompt — bookkeeping after the reply doesn't count as
    /// "still running" for draft purposes.
    #[test]
    fn reply_anchored_pulls_the_clock_forward_to_the_reply() {
        assert_eq!(
            reply_anchored(Some(Duration::from_secs(60)), Some(Duration::from_secs(10))),
            Some(Duration::from_secs(10))
        );
    }

    /// No reply yet this spell: the anchor is a no-op, same as before fix.
    #[test]
    fn reply_anchored_passes_through_with_no_reply() {
        assert_eq!(
            reply_anchored(Some(Duration::from_secs(60)), None),
            Some(Duration::from_secs(60))
        );
    }

    /// Not working at all: nothing to anchor.
    #[test]
    fn reply_anchored_passes_through_when_not_working() {
        assert_eq!(reply_anchored(None, Some(Duration::from_secs(10))), None);
    }

    /// Defensive: a reply age that is not shorter than the working age (it should never
    /// predate the current spell — `status.rs` resets it on every `UserPrompt` — but this
    /// function does not itself know that) must not push the clock *backward*.
    #[test]
    fn reply_anchored_never_moves_the_clock_backward() {
        assert_eq!(
            reply_anchored(Some(Duration::from_secs(10)), Some(Duration::from_secs(60))),
            Some(Duration::from_secs(10))
        );
    }

    /// Privacy guard on the wire format, mirroring the one in `status`.
    #[test]
    fn rendered_bodies_are_metadata_only() {
        let working = render_working(&status(AgentState::Working));
        assert!(working.contains("Bash"));
        assert!(working.contains("working"));

        let terminal = render_terminal(&status(AgentState::Stalled));
        assert!(terminal.contains("stalled"));
        assert!(terminal.contains("3m 20s"));
    }

    #[test]
    fn bodies_respect_the_length_cap() {
        let long = truncate("x".repeat(MAX_TOTAL_LENGTH + 5_000));
        assert_eq!(long.len(), MAX_TOTAL_LENGTH);
    }

    /// Log into the throwaway `MATRIX_TEST_*` account and create a fresh room, shared by
    /// the ignored live-homeserver tests below. Panics with a clear message if the env
    /// vars aren't set.
    ///
    /// Returns the `TempDir` alongside the client — the sqlite store lives there, and the
    /// caller must keep it alive for as long as the client is in use (bind it, even if
    /// unused directly: `let (client, room_id, _store) = homeserver_test_client().await;`).
    async fn homeserver_test_client() -> (Client, OwnedRoomId, tempfile::TempDir) {
        let (Ok(hs), Ok(user), Ok(pass)) = (
            std::env::var("MATRIX_TEST_HOMESERVER"),
            std::env::var("MATRIX_TEST_USER"),
            std::env::var("MATRIX_TEST_PASSWORD"),
        ) else {
            panic!("set MATRIX_TEST_HOMESERVER / _USER / _PASSWORD");
        };

        let store = tempfile::tempdir().unwrap();
        let client = Client::builder()
            .homeserver_url(&hs)
            .sqlite_store(store.path(), None)
            .build()
            .await
            .expect("client build");

        let localpart = user.trim_start_matches('@').split(':').next().unwrap();
        client
            .matrix_auth()
            .login_username(localpart, &pass)
            .initial_device_display_name("cc-matrix-channel-livetest")
            .await
            .expect("login");
        println!("logged in as {}", client.user_id().unwrap());

        client.sync_once(Default::default()).await.expect("sync");

        let room = client
            .create_room(matrix_sdk::ruma::api::client::room::create_room::v3::Request::new())
            .await
            .expect("create room");
        let room_id = room.room_id().to_owned();
        println!("test room: {room_id}");

        (client, room_id, store)
    }

    /// Exercises the real Matrix write path against a live homeserver: send a draft, edit
    /// it in place twice, then close with a *new* message. The unit tests above only prove
    /// `decide()` picks the right action — this proves the actions actually work.
    ///
    /// Requires a throwaway account. Credentials come from the environment and are never
    /// written to disk; the store goes to a temp dir, never the live one.
    ///
    ///   MATRIX_TEST_HOMESERVER=... MATRIX_TEST_USER=... MATRIX_TEST_PASSWORD=... \
    ///     cargo test live_draft_cycle -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires a throwaway Matrix account"]
    async fn live_draft_cycle_against_real_homeserver() {
        let (client, room_id, _store) = homeserver_test_client().await;

        // 1. Open the draft.
        let working = render_working(&status(AgentState::Working));
        let event_id = send_status(&client, &room_id, &working)
            .await
            .expect("draft send should return an event id");
        println!("draft event: {event_id}");

        // 2. Edit it in place. This is the OpenClaw/Hermes behaviour Sky asked for.
        for n in 1..=2 {
            let body = format!("{working}\nEdit #{n}");
            assert!(
                edit_status(&client, &room_id, &event_id, &body).await,
                "edit #{n} should be accepted by the homeserver"
            );
            println!("edit #{n} accepted");
        }

        // 3. Close with a NEW message, because Matrix edits do not push-notify.
        let terminal = render_terminal(&status(AgentState::Stalled));
        let terminal_id = send_status(&client, &room_id, &terminal)
            .await
            .expect("terminal send should return an event id");
        println!("terminal event: {terminal_id}");

        assert_ne!(
            event_id, terminal_id,
            "the terminal message must be a new event, not an edit of the draft"
        );

        client.matrix_auth().logout().await.ok();
    }

    /// Confirms `post_fallback_chunks` actually lands a message via a real homeserver —
    /// the piece `should_post_fallback`/`extract_last_turn_text`'s unit tests in
    /// `fallback_reply.rs` can't cover, since they're deliberately matrix-free.
    ///
    ///   MATRIX_TEST_HOMESERVER=... MATRIX_TEST_USER=... MATRIX_TEST_PASSWORD=... \
    ///     cargo test missed_reply_fallback_posts -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires a throwaway Matrix account"]
    async fn missed_reply_fallback_posts_recovered_text_against_real_homeserver() {
        let (client, room_id, _store) = homeserver_test_client().await;

        let sent = post_fallback_chunks(
            &client,
            &room_id,
            "the answer that never went through mcp__matrix__reply",
            4096,
            &ChunkMode::Newline,
        )
        .await;
        assert_eq!(sent, 1, "a short message should land as a single chunk");

        client.matrix_auth().logout().await.ok();
    }
}
