# Missed-Reply Fallback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a Matrix-bridge turn ends without ever calling `mcp__matrix__reply`, the bridge automatically posts the turn's recovered answer text into the room as a backstop, without changing how the reply tool works for every other turn.

**Architecture:** A new matrix-free module, `src/fallback_reply.rs`, adds two pure-ish functions: `should_post_fallback` (metadata-only trigger, mirrors the existing `AgentState`/`last_reply_age` machinery in `status.rs`) and `extract_last_turn_text` (the one place in the codebase that reads assistant `text` content, isolated from `status.rs`'s no-text-read guarantee). `live_status.rs`'s existing tick loop calls both once per turn and, on trigger, posts the recovered text through the same room-resolution and chunking machinery `mcp.rs::reply` already uses for explicit replies.

**Tech Stack:** Rust (edition 2024), `matrix-sdk`, `serde_json`, existing `tokio`-based tick loop. Build/test via `source /workspace/vela-handoff/tools/env.sh` (durable Zig-backed toolchain — see that file's header comment; this container has no C compiler or root).

## Global Constraints

- Never weaken `status.rs`'s existing rule that message text is never read into `AgentStatus` — all new text-reading lives in `fallback_reply.rs` only.
- This is a safety-net backstop, not a replacement for `mcp__matrix__reply` — do not change when or whether the agent calls the reply tool.
- No visible "auto-posted" tag on the fallback message body (Sky's explicit call in design review) — it posts plainly, like a normal reply.
- No reply-to-event threading in this round.
- No Matrix-vs-local-turn origin check — confirmed with Sky that local attachment to the bridge's own session doesn't happen in practice, so every `UserPrompt` in the watched transcript is already Matrix-sourced.
- Fallback posting must use `AccessControl`'s configured `text_chunk_limit`/`chunk_mode` (chunked, like `reply`), not `live_status.rs`'s `truncate()` (which silently cuts off).
- `cargo fmt --check` and `cargo clippy --all-targets` must stay at the baseline established below — no new warnings beyond the pre-existing `access.rs` ones.
- Design spec: `docs/superpowers/specs/2026-08-10-missed-reply-fallback-design.md` — read it for the rejected alternatives (visible tagging, origin-marker check, alert-only) and why.

**Baseline, verified before this plan was written:** `cargo test --release` on this branch (`feat/missed-reply-fallback`, based on `009f216`) passes **70 passed, 0 failed, 2 ignored** — matches the last recorded state in `HANDOFF.md`. Both ignored tests need live infra (`live_session_reports_a_real_state` needs a live Claude Code session; `live_draft_cycle_against_real_homeserver` needs `MATRIX_TEST_HOMESERVER`/`_USER`/`_PASSWORD`) and are expected to stay ignored through this plan.

---

### Task 1: `should_post_fallback` — pure trigger

**Files:**
- Create: `src/fallback_reply.rs`
- Modify: `src/main.rs:1-7` (register the new module)
- Test: inline `#[cfg(test)] mod tests` in `src/fallback_reply.rs`

**Interfaces:**
- Consumes: `crate::status::AgentState` (already `pub`, has `PartialEq, Eq, Clone, Copy, Debug`).
- Produces: `pub(crate) fn should_post_fallback(previous: Option<AgentState>, current: AgentState, last_reply_age: Option<Duration>) -> bool` — Task 3 calls this from the tick loop.

- [ ] **Step 1: Register the module**

In `src/main.rs`, the module list is alphabetical:

```rust
mod access;
mod config;
mod live_status;
mod matrix;
mod mcp;
mod rooms;
mod status;
```

Insert `mod fallback_reply;` between `config` and `live_status`:

```rust
mod access;
mod config;
mod fallback_reply;
mod live_status;
mod matrix;
mod mcp;
mod rooms;
mod status;
```

- [ ] **Step 2: Write the failing tests**

Create `src/fallback_reply.rs` with just the module doc, imports, and tests (no implementation yet):

```rust
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
```

- [ ] **Step 3: Run tests to verify they fail**

```bash
source /workspace/vela-handoff/tools/env.sh
cd /workspace/vela-handoff/tools/cc_matrix_channel
cargo test --release fallback_reply 2>&1 | tail -20
```

Expected: compile error, `cannot find function 'should_post_fallback' in this scope`.

- [ ] **Step 4: Implement `should_post_fallback`**

Add above the `#[cfg(test)]` block in `src/fallback_reply.rs`:

```rust
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
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test --release fallback_reply 2>&1 | tail -20
```

Expected: 4 tests pass (`fires_on_entering_waiting_for_user_with_no_reply`, `does_not_fire_when_a_reply_landed`, `does_not_refire_while_still_waiting`, `does_not_fire_for_non_waiting_states`).

- [ ] **Step 6: Commit**

```bash
git add src/main.rs src/fallback_reply.rs
git commit -m "feat: add should_post_fallback trigger for the missed-reply backstop

Pure, metadata-only decision mirroring the debounce idiom live_status::decide
already uses for terminal states. No text reading yet — that's the next task."
```

---

### Task 2: `extract_last_turn_text` — text recovery

**Files:**
- Modify: `src/status.rs:281` (`read_tail` visibility)
- Modify: `src/fallback_reply.rs` (add the function + tests)

**Interfaces:**
- Consumes: `crate::status::read_tail(path: &Path) -> Option<String>` (bumped from private to `pub(crate)` in this task).
- Produces: `pub(crate) fn extract_last_turn_text(path: &Path) -> Option<String>` — Task 3 calls this from the tick loop, passed the transcript path already resolved each tick.

- [ ] **Step 1: Bump `read_tail` visibility**

In `src/status.rs`, line 281 currently reads:

```rust
fn read_tail(path: &Path) -> Option<String> {
```

Change to:

```rust
pub(crate) fn read_tail(path: &Path) -> Option<String> {
```

- [ ] **Step 2: Write the failing tests**

Append to `src/fallback_reply.rs`, above the closing brace of the existing `mod tests` block (keep it in the same test module, alongside the Task 1 tests):

```rust
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

    const PROMPT: &str =
        r#"{"type":"user","timestamp":"2026-08-08T12:00:00.000Z","message":{"content":[{"type":"text","text":"do the thing"}]}}"#;
    const PROMPT2: &str =
        r#"{"type":"user","timestamp":"2026-08-08T12:01:00.000Z","message":{"content":[{"type":"text","text":"do another thing"}]}}"#;
    const TEXT1: &str =
        r#"{"type":"assistant","timestamp":"2026-08-08T12:00:05.000Z","message":{"content":[{"type":"text","text":"first part"}]}}"#;
    const TEXT2: &str =
        r#"{"type":"assistant","timestamp":"2026-08-08T12:00:09.000Z","message":{"content":[{"type":"text","text":"second part"}]}}"#;
    const THINKING: &str =
        r#"{"type":"assistant","timestamp":"2026-08-08T12:00:03.000Z","message":{"content":[{"type":"thinking","thinking":"internal reasoning nobody should see","signature":"x"}]}}"#;
    const TOOL_USE: &str =
        r#"{"type":"assistant","timestamp":"2026-08-08T12:00:07.000Z","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#;
    const TOOL_RESULT: &str =
        r#"{"type":"user","timestamp":"2026-08-08T12:00:08.000Z","toolUseResult":{"stdout":"ok"},"message":{"content":[{"type":"tool_result"}]}}"#;

    #[test]
    fn single_text_block_is_returned() {
        assert_eq!(
            extract_of(&[PROMPT, TEXT1]),
            Some("first part".to_string())
        );
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
```

- [ ] **Step 3: Run tests to verify they fail**

```bash
cargo test --release fallback_reply 2>&1 | tail -30
```

Expected: compile error, `cannot find function 'extract_last_turn_text' in this scope`.

- [ ] **Step 4: Implement `extract_last_turn_text`**

Add above the `#[cfg(test)]` block in `src/fallback_reply.rs` (after `should_post_fallback`):

```rust
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
```

`TAIL_BYTES` itself stays private (`const TAIL_BYTES: u64 = 64 * 1024;` in `status.rs`) — only mentioned in prose above, not linked or referenced as code, so no further visibility change is needed beyond `read_tail` in Step 1.

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test --release fallback_reply 2>&1 | tail -30
```

Expected: all 10 tests in `fallback_reply::tests` pass (4 from Task 1, 6 from this task).

- [ ] **Step 6: Commit**

```bash
git add src/status.rs src/fallback_reply.rs
git commit -m "feat: add extract_last_turn_text for the missed-reply backstop

The one place in the codebase that reads assistant text content — thinking
blocks and tool I/O stay untouched, per status.rs's existing privacy rule."
```

---

### Task 3: Wire into the live-status tick loop

**Files:**
- Modify: `src/mcp.rs:1021` (`chunk_message` visibility)
- Modify: `src/mcp.rs` (call site of `crate::live_status::spawn`, near line 1125)
- Modify: `src/main.rs` (call site of `live_status::spawn`, near line 193)
- Modify: `src/live_status.rs` (imports, `spawn` signature, new `post_fallback_chunks` fn, tick-loop call site, new `homeserver_test_client` test helper shared with the existing ignored test, new ignored integration test)

**Interfaces:**
- Consumes: `fallback_reply::should_post_fallback`, `fallback_reply::extract_last_turn_text` (Tasks 1–2); `crate::mcp::chunk_message(text: &str, max_size: usize, mode: &ChunkMode) -> Vec<&str>` (bumped to `pub(crate)` in this task); `AccessControl::text_chunk_limit(&self) -> usize` and `AccessControl::chunk_mode(&self) -> ChunkMode` (already `pub`, in `src/access.rs`); `target_room` (already private to `live_status.rs`, reused as-is).
- Produces: `live_status::spawn` gains a 5th parameter `access_control: Arc<AccessControl>`, inserted before `cancel` — every caller must be updated (there are exactly two, both in this task).

- [ ] **Step 1: Bump `chunk_message` visibility**

In `src/mcp.rs`, line 1021 currently reads:

```rust
fn chunk_message<'a>(text: &'a str, max_size: usize, mode: &ChunkMode) -> Vec<&'a str> {
```

Change to:

```rust
pub(crate) fn chunk_message<'a>(text: &'a str, max_size: usize, mode: &ChunkMode) -> Vec<&'a str> {
```

- [ ] **Step 2: Update `live_status.rs` imports and add `post_fallback_chunks`**

In `src/live_status.rs`, the import block currently reads:

```rust
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use matrix_sdk::Client;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
use tokio_util::sync::CancellationToken;

use crate::status::{AgentState, AgentStatus, TEXT_GRACE, read_status, stall_threshold};
```

Add three more `use` lines after the last one:

```rust
use crate::access::{AccessControl, ChunkMode};
use crate::fallback_reply::{extract_last_turn_text, should_post_fallback};
use crate::mcp::chunk_message;
```

Then, directly after the existing `edit_status` function (right before `/// Spawn the live-status loop. Returns immediately.`), add:

```rust
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
```

- [ ] **Step 3: Add the `access_control` parameter to `spawn`**

In `src/live_status.rs`, the signature currently reads:

```rust
pub fn spawn(
    client: Arc<Client>,
    known_rooms: Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>>,
    last_active_room: Arc<parking_lot::Mutex<Option<OwnedRoomId>>>,
    cancel: CancellationToken,
) {
```

Change to:

```rust
pub fn spawn(
    client: Arc<Client>,
    known_rooms: Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>>,
    last_active_room: Arc<parking_lot::Mutex<Option<OwnedRoomId>>>,
    access_control: Arc<AccessControl>,
    cancel: CancellationToken,
) {
```

`access_control` is captured by the `tokio::spawn(async move { ... })` block below it automatically, the same way `client`/`known_rooms`/`last_active_room` already are — no further change needed to the `async move` line itself.

- [ ] **Step 4: Call `should_post_fallback`/`extract_last_turn_text`/`post_fallback_chunks` from the tick loop**

In `src/live_status.rs`, inside the `loop { ... }` in `spawn`, find the end of the `match action { ... }` block, immediately followed by:

```rust
            previous = Some(status.state);
        }
```

Insert the fallback check between the closing `}` of `match action` and `previous = Some(status.state);`:

```rust
            if should_post_fallback(previous, status.state, status.last_reply_age) {
                match resolved.as_ref().and_then(|(path, _)| extract_last_turn_text(path)) {
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
                        None => tracing::info!(
                            "Missed-reply fallback triggered but no target room yet"
                        ),
                    },
                    None => tracing::info!(
                        "Missed-reply fallback triggered but no text recovered"
                    ),
                }
            }

            previous = Some(status.state);
        }
```

`resolved` is the `Option<(PathBuf, TranscriptSource)>` already computed earlier in the same loop iteration (`let resolved = crate::status::transcript_path_with_source();`) — reuse it rather than re-resolving.

- [ ] **Step 5: Update both call sites**

In `src/main.rs`, the block currently reads:

```rust
    if let Some(client) = matrix_client.clone() {
        live_status::spawn(
            client,
            known_rooms.clone(),
            last_active_room.clone(),
            cancel.clone(),
        );
    }
```

Change to:

```rust
    if let Some(client) = matrix_client.clone() {
        live_status::spawn(
            client,
            known_rooms.clone(),
            last_active_room.clone(),
            access_control.clone(),
            cancel.clone(),
        );
    }
```

In `src/mcp.rs`, the block currently reads:

```rust
        crate::live_status::spawn(
            client,
            self.known_rooms.clone(),
            self.last_active_room.clone(),
            self.cancel.clone(),
        );
```

Change to:

```rust
        crate::live_status::spawn(
            client,
            self.known_rooms.clone(),
            self.last_active_room.clone(),
            self.access_control.clone(),
            self.cancel.clone(),
        );
```

- [ ] **Step 6: Compile**

```bash
source /workspace/vela-handoff/tools/env.sh
cd /workspace/vela-handoff/tools/cc_matrix_channel
cargo build --release 2>&1 | tail -40
```

Expected: clean build, no errors. This is the step that actually proves the wiring (types, field names, both call sites) is correct — a mismatched parameter list or missing field would fail here.

- [ ] **Step 7: Extract a shared homeserver-test-client helper**

Two ignored tests in `src/live_status.rs`'s `#[cfg(test)] mod tests` will now need the same login-and-create-a-room preamble (the existing `live_draft_cycle_against_real_homeserver` and the new one added in Step 8). Extract it once rather than duplicating it a second time.

The existing test currently opens with:

```rust
    async fn live_draft_cycle_against_real_homeserver() {
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

        // 1. Open the draft.
```

Add a helper function in the same `mod tests` block, above `live_draft_cycle_against_real_homeserver`:

```rust
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
```

Then replace the quoted preamble in `live_draft_cycle_against_real_homeserver` with:

```rust
    async fn live_draft_cycle_against_real_homeserver() {
        let (client, room_id, _store) = homeserver_test_client().await;

        // 1. Open the draft.
```

Run the (non-ignored) suite to confirm nothing else broke — the ignored test itself can't be exercised without live credentials, but this at least confirms the refactor compiles and every other test is unaffected:

```bash
source /workspace/vela-handoff/tools/env.sh
cd /workspace/vela-handoff/tools/cc_matrix_channel
cargo test --release 2>&1 | tail -15
```

Expected: same 70 passed, 0 failed, 2 ignored as the baseline — this step only refactors test setup, adds nothing new yet.

- [ ] **Step 8: Add the new ignored live-homeserver integration test**

In the same `mod tests` block, directly after `live_draft_cycle_against_real_homeserver`, add:

```rust
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
```

This test stays `#[ignore]`d in CI/normal runs — same as `live_draft_cycle_against_real_homeserver` and `status::tests::live_session_reports_a_real_state` already are. Not run as part of this plan's verification (no throwaway Matrix account available here); documented so it can be run manually later.

- [ ] **Step 9: Run the full test suite**

```bash
cargo test --release 2>&1 | tail -15
```

Expected: passed count is 70 (baseline) + 4 (Task 1) + 6 (Task 2) = **80 passed**, 0 failed, **3 ignored** (2 baseline + the new homeserver test).

- [ ] **Step 10: Commit**

```bash
git add src/mcp.rs src/main.rs src/live_status.rs
git commit -m "feat: post a missed-reply fallback from the live-status tick loop

Wires should_post_fallback/extract_last_turn_text into the existing tick
loop as an additive effect alongside the current draft/alert actions.
Chunks through the same access-control-configured limit reply() uses.

Adds an #[ignore]d live-homeserver test for post_fallback_chunks, matching
the existing live_draft_cycle_against_real_homeserver precedent."
```

---

### Task 4: Validate against the real known misses

**Files:**
- Create: `/workspace/vela-handoff/tools/fallback-check/Cargo.toml`
- Create: `/workspace/vela-handoff/tools/fallback-check/src/main.rs`

This mirrors `tools/spellclock`'s approach exactly: compile the **real** `status.rs` and `fallback_reply.rs` via `#[path]` (not copies) so the check cannot drift from the code under test, and run them against the real transcripts `tools/missed-reply-scan.mjs` already identified as the 4 known misses.

**Interfaces:**
- Consumes: `fallback_reply::should_post_fallback`, `fallback_reply::extract_last_turn_text`, `status::read_status_at`, `status::AgentState` — all from Tasks 1–3, included by path.
- Produces: a standalone CLI, not consumed by any later task — this is a validation tool, not library code.

- [ ] **Step 1: Confirm the current known misses**

```bash
cd /workspace/vela-handoff/tools
node missed-reply-scan.mjs
```

As of this plan being written, this prints 4 misses. One (`15594446-…`, "turn superseded by a new prompt before ending") is a structurally different case — the turn was interrupted by a new prompt rather than reaching a clean terminal state, so it never enters `AgentState::WaitingForUser` at all and `should_post_fallback` correctly does **not** catch it; that's expected, not a gap in this feature. The other 3 all have `stop_reason=end_turn`, the shape this feature targets:

- `69f4832e-e49f-4195-9ec0-31c4802a75cd.jsonl`
- `d1a4e472-98f4-495c-9c5b-1d11e668af9e.jsonl`
- `ec577585-47ec-4601-bde3-172ba8646514.jsonl`

If the list differs when this task is actually run (new misses since this plan was written, or these three no longer present), use whatever `missed-reply-scan.mjs` reports at run time instead — it's the ground truth, not this plan.

- [ ] **Step 2: Create the harness project**

```bash
mkdir -p /workspace/vela-handoff/tools/fallback-check/src
```

`/workspace/vela-handoff/tools/fallback-check/Cargo.toml`:

```toml
[package]
name = "fallback-check"
version = "0.1.0"
edition = "2024"

[dependencies]
serde_json = "1"

[[bin]]
name = "fallback-check"
path = "src/main.rs"
```

`/workspace/vela-handoff/tools/fallback-check/src/main.rs`:

```rust
//! Validates `should_post_fallback` / `extract_last_turn_text` against real transcripts
//! holding known missed-reply cases (see `tools/missed-reply-scan.mjs`), using the REAL
//! `status.rs` and `fallback_reply.rs` from the fork (no copy, no edit) so this cannot
//! drift from the code it is checking.
//!
//! Relative to this file, so it survives being moved between machines: clone the repo to
//! `tools/cc_matrix_channel` (a sibling of `tools/fallback-check`) first, same as
//! `tools/spellclock` requires.
//!
//!   cargo run --release -- /path/to/transcript.jsonl

#[path = "../../cc_matrix_channel/src/status.rs"]
mod status;
#[path = "../../cc_matrix_channel/src/fallback_reply.rs"]
mod fallback_reply;

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use fallback_reply::{extract_last_turn_text, should_post_fallback};
use status::{AgentState, read_status_at};

const STALL: Duration = Duration::from_secs(300);

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: fallback-check <transcript.jsonl>");
    let path = PathBuf::from(path);

    let status = read_status_at(&path, STALL, SystemTime::now());
    println!("state: {:?}", status.state);
    println!("last_reply_age: {:?}", status.last_reply_age);

    // The tick loop's real `previous` isn't recoverable from a single static snapshot —
    // this simulates the realistic edge tick (a turn transitions Working -> terminal),
    // which is what every genuine miss looks like.
    let would_trigger = should_post_fallback(Some(AgentState::Working), status.state, status.last_reply_age);
    println!("would_trigger (Working -> current, edge tick): {would_trigger}");

    match extract_last_turn_text(&path) {
        Some(text) => println!(
            "recovered text: {} chars, starts: {:?}",
            text.len(),
            text.chars().take(80).collect::<String>()
        ),
        None => println!("recovered text: none"),
    }
}
```

- [ ] **Step 3: Build it**

```bash
source /workspace/vela-handoff/tools/env.sh
cd /workspace/vela-handoff/tools/fallback-check
cargo build --release 2>&1 | tail -30
```

Expected: clean build. If it fails on `pub(crate)` visibility for anything `fallback_reply.rs` or `status.rs` reaches into, that's a real signal — every item they use across the module boundary must already be `pub(crate)` or `pub` from Tasks 1–2; nothing new should be needed here since this harness includes both files as siblings under one crate root exactly the way the real crate does.

- [ ] **Step 4: Run it against the 3 known misses**

```bash
for f in 69f4832e-e49f-4195-9ec0-31c4802a75cd \
         d1a4e472-98f4-495c-9c5b-1d11e668af9e \
         ec577585-47ec-4601-bde3-172ba8646514; do
  echo "=== $f ==="
  ./target/release/fallback-check "/home/node/.claude/projects/-workspace/$f.jsonl"
done
```

Expected for all three: `state: WaitingForUser`, `last_reply_age: None`, `would_trigger (Working -> current, edge tick): true`, and non-empty recovered text. If `state` is not `WaitingForUser` for one of them, the transcript has moved on since this plan was written (a later turn appended) — re-run `missed-reply-scan.mjs` and use whatever it currently reports instead, per Step 1's note.

- [ ] **Step 5: Spot-check the recovered text for the freshest miss**

`HANDOFF.md` already recorded the known answer for `ec577585-…` by hand ("yes, running `009f216`/`cc_matrix_channel-009f216`, not yet pushed to `origin`"). Confirm the harness's output for that file contains recognizable content from that answer (allowing for the transcript having grown since — check for the substring, not an exact match):

```bash
./target/release/fallback-check "/home/node/.claude/projects/-workspace/ec577585-47ec-4601-bde3-172ba8646514.jsonl" | grep -i "009f216"
```

Expected: a match. This is the one point in the plan that ties the abstract "recovered text" back to a human-readable answer already known to be correct.

- [ ] **Step 6: Commit**

```bash
cd /workspace/vela-handoff/tools/cc_matrix_channel
git -C /workspace/vela-handoff/tools/fallback-check init -q 2>/dev/null || true
```

`tools/fallback-check` lives under `/workspace/vela-handoff`, not inside the `cc_matrix_channel` git repo (same as `tools/spellclock`) — it isn't part of the feature branch's history. No commit needed in the feature repo for this task; the durability comes from `/workspace` itself (see `HANDOFF.md`'s "Why this directory exists"). Skip this step's git actions if `tools/fallback-check` was created directly under `/workspace/vela-handoff/tools/` as instructed above (it already is durable).

---

### Task 5: Full verification pass

**Files:** none created or modified — this task only runs checks.

**Interfaces:** none — terminal task.

- [ ] **Step 1: Full test suite**

```bash
source /workspace/vela-handoff/tools/env.sh
cd /workspace/vela-handoff/tools/cc_matrix_channel
cargo test --release 2>&1 | tail -15
```

Expected: **80 passed, 0 failed, 3 ignored** (see Task 3 Step 8's math).

- [ ] **Step 2: Format check**

```bash
cargo fmt --check 2>&1
```

Expected: no output, exit code 0. If it reports diffs, run `cargo fmt` and re-review the specific files it touched before committing — don't blanket-accept formatting changes to files this plan didn't intend to touch.

- [ ] **Step 3: Clippy**

```bash
cargo clippy --all-targets --release 2>&1 | tail -40
```

Expected: only the pre-existing warnings in `access.rs` (per `HANDOFF.md`: "Clippy warnings exist only in `access.rs` and predate this work"). Anything new in `fallback_reply.rs`, `live_status.rs`, `mcp.rs`, `status.rs`, or `main.rs` must be fixed before proceeding.

- [ ] **Step 4: Confirm branch state**

```bash
git log --oneline -6
git status -sb
```

Expected: 4 new commits on `feat/missed-reply-fallback` (Tasks 1–3's three feature commits plus the spec-doc commit already made during brainstorming), clean working tree, branch still based on `009f216` (the currently-deployed commit).

- [ ] **Step 5: Report status, do not deploy**

Deployment (building the release binary, updating `.mcp.json`, restarting the bridge) is a separate, explicit decision — see `HANDOFF.md`'s deploy history for why (every prior deploy in this project was done with Sky's explicit go-ahead, never automatically). Stop here and report: test/fmt/clippy results, the validation output from Task 4, and that the branch is ready for review — then wait for Sky's decision on merging/deploying.

## Self-review notes

- **Spec coverage:** every section of `docs/superpowers/specs/2026-08-10-missed-reply-fallback-design.md` maps to a task — architecture/module split (Tasks 1–2), detection (Task 1), extraction (Task 2), posting (Task 3), testing (Tasks 1–3), out-of-scope items are simply not built (verified by their absence: no tag string anywhere, no `reply_to_event_id` param on `post_fallback_chunks`, no origin-marker check in `extract_last_turn_text`).
- **Type consistency checked:** `should_post_fallback(previous: Option<AgentState>, current: AgentState, last_reply_age: Option<Duration>) -> bool` and `extract_last_turn_text(path: &Path) -> Option<String>` are used with the same signatures in Task 1/2's own tests, Task 3's call site, and Task 4's harness.
- **No placeholders:** every step above has literal code, not a description of code.
