# Missed-reply fallback — design

Written 2026-08-10 by Vela, brainstormed with Sky. Builds on the live-status
investigation in `/workspace/vela-handoff/HANDOFF.md` — read that first for the
`AgentState`/`AgentStatus` machinery this depends on.

## Problem

`tools/missed-reply-scan.mjs` (offline scanner) found 4 confirmed cases where the agent
did real work, produced a correct answer, and never called `mcp__matrix__reply` — the
answer landed as plain assistant text and never reached the Matrix room. One instance was
minutes after a live redeploy, still unanswered at time of writing. Same shape each time:
real work happened, `stop_reason` ended the turn, no `mcp__matrix__reply` tool call in
between.

## Scope decision

This is a **safety-net backstop**, not a replacement for the reply tool. The reply tool
still matters for the case fixes 3/4 (`reply_anchored`, deferred-send hold) exist for:
replying mid-turn and continuing to work afterward, which a pure end-of-turn fallback
cannot replicate — it only ever fires once a turn is fully over. The agent should keep
calling `mcp__matrix__reply` normally; this only catches the case where it never did.

Alerting-only (flag the miss without the text) was considered and rejected: with the
scope narrowed to "safety net," posting the actual answer is strictly more useful than a
bare warning, and Sky doesn't need it visibly tagged as a bridge intervention (see
"Posting" below) — so there is no reason to withhold the content once the trigger has
already fired.

## Origin signal: not needed

An earlier version of this design considered distinguishing Matrix-sourced turns from
turns typed directly into the bridge's own session (e.g. via `tmux attach -d -t local`),
since only Matrix-sourced turns have a room waiting on them. Confirmed with Sky: local
attachment to the bridge's own session doesn't happen for debugging in practice — that
happens from a separate session. So every `UserPrompt` in the bridge's watched transcript
is already Matrix-sourced in practice, and no origin check is needed. If this assumption
ever stops holding, revisit — the `<channel source="matrix">` marker check considered and
dropped here is documented in the session history if needed.

## Architecture

A new module, `src/fallback_reply.rs`, kept separate from `status.rs`.

`status.rs`'s module doc currently states message text is "never read out of the
transcript and must never be added to `AgentStatus`" — that guarantee stays intact.
`fallback_reply.rs` is the one place in the codebase that reads actual message text, and
only *text* content blocks (never tool inputs/outputs, never `thinking` blocks), and only
after the trigger has already been decided from pure metadata (`AgentState`,
`last_reply_age`) exactly the way the rest of the bridge decides everything else.

### Detection

A new pure function, independent of `live_status.rs`'s existing `decide()`:

```rust
fn should_post_fallback(
    previous: Option<AgentState>,
    current: AgentState,
    last_reply_age: Option<Duration>,
) -> bool {
    current == AgentState::WaitingForUser
        && previous != Some(AgentState::WaitingForUser)   // debounce: once per turn
        && last_reply_age.is_none()                        // reply tool never landed
}
```

This mirrors the debounce idiom `decide()` already uses for `is_terminal()` states
(`previous == Some(s)` → `Nothing`), computed alongside `decide()`'s result in the tick
loop rather than folded into it. `CloseDraft` / `AlertOnly` / `Nothing` all still fire
exactly as they do today for the primary status message — fallback-posting is an
*additional* effect of the same tick, not a replacement branch. No new state needed:
`previous` is already threaded through the loop for `decide()`'s own use.

Debounce correctness: `previous` only equals `Some(WaitingForUser)` after this same check
has already fired once for the turn (or the turn ended in `WaitingForUser` before this
feature existed and the loop restarted — a one-time gap, not a recurring one, since a new
`UserPrompt` moves `current` to `Working` and resets `previous` to `Some(Working)` before
the next `WaitingForUser` entry).

### Extraction

```rust
fn extract_last_turn_text(transcript_path: &Path) -> Option<String>
```

Reads the same tail window `status.rs` already reads (`TAIL_BYTES`), finds the current
turn's `UserPrompt` record, concatenates every subsequent assistant **`text`** content
block (never `thinking`) in encounter order, separated by blank lines. Returns `None` if
there is no text at all (turn ended purely on tool calls with no trailing text) — nothing
gets posted in that case, since there'd be nothing useful to relay.

**Known limitation:** same tail-window bound `AgentStatus::turn_elapsed` already accepts —
an exceptionally long turn where the prompt has scrolled out of the `TAIL_BYTES` window
could lose leading text. Accepted as consistent with existing precedent, not solved here.

### Posting

- Room: `target_room(&known_rooms, &last_active_room)` — the same function status/alert
  messages already use, carrying the same containment guarantee as `check_outbound_gate`
  (only rooms that have already talked to the bridge).
- Chunking: reuses `chunk_message` (already used by `mcp.rs::reply`) via a small shared
  helper, rather than `live_status.rs`'s `truncate()` — a long fallback answer should be
  chunked, not silently cut off, since delivering the actual answer is the point.
- Shape: a plain new message, posted as the agent would post it via `reply` — no visible
  "auto-posted" tag (Sky's call: the room stays clean; observability moves to the log
  line below instead).
- No reply-threading to the originating Matrix event — `reply_to_event_id` needs an
  event id this code path doesn't otherwise track. Out of scope for v1; can be added
  later if it turns out to matter.
- A `tracing::info!` log line fires whenever this triggers, so the failure mode (agent
  forgetting to call reply) stays visible server-side even though the room copy is
  indistinguishable from a normal reply.

## Testing

Unit tests, no live homeserver needed for the logic itself:

- `should_post_fallback`: fires once on entering `WaitingForUser` with no reply; does not
  re-fire on subsequent ticks in the same state (debounce); does not fire when
  `last_reply_age` is `Some` (reply landed, mid-turn or otherwise).
- `extract_last_turn_text`: concatenates multiple text blocks across tool calls in order;
  excludes `thinking` blocks; returns `None` when the turn has no text content at all.
- Posting path: exercised similarly to `live_draft_cycle_against_real_homeserver` if a
  homeserver-backed test is warranted for the chunking helper — decide during
  implementation whether this needs new coverage or can share the existing one.

## Out of scope (this round)

- Visible "auto-posted" tagging.
- Reply-to-event threading.
- Origin distinction between Matrix-sourced and local turns (see "Origin signal" above).
- Making the reply tool optional for end-of-turn answers — considered, explicitly
  deferred; this stays a backstop, not the default delivery path.
