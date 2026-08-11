use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use matrix_sdk::{
    Client, LoopCtrl, Room,
    config::{RequestConfig, SyncSettings},
    ruma::{
        OwnedDeviceId, OwnedEventId, OwnedRoomId, OwnedUserId,
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            reaction::{OriginalSyncReactionEvent, ReactionEventContent},
            relation::Annotation,
            room::{
                member::StrippedRoomMemberEvent,
                message::{
                    MessageType, OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent,
                },
            },
        },
        serde::Raw,
    },
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::access::{AccessControl, AccessDenied};
use crate::config::Config;

/// Upper bound on how long the SDK may keep retrying a single failed request.
///
/// `RequestConfig::default()` leaves `retry_timeout` unset, which becomes
/// `ExponentialBackoff { max_elapsed_time: None }` — i.e. retry forever. A
/// transient 5xx on any request then never returns, and if that request was
/// awaited from an event handler it stalls the whole sync loop silently.
const REQUEST_RETRY_TIMEOUT: Duration = Duration::from_secs(60);

/// Outbound side effects (typing notices, reactions) are detached from the sync
/// loop, but still shouldn't pile up forever if the homeserver is unhealthy.
const SIDE_EFFECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the MCP side to accept a notification before giving up
/// and logging. Bounded so a stalled consumer cannot wedge sync.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the sync loop may go without completing a single sync before the
/// watchdog declares it stalled and restarts it. The SDK's default sync timeout
/// is 30s, so a healthy loop checks in far more often than this.
const SYNC_STALL_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the watchdog compares the sync heartbeat against the wall clock.
const WATCHDOG_TICK: Duration = Duration::from_secs(15);

/// Delay before re-entering the sync loop after it stalled or returned an error.
const SYNC_RESTART_DELAY: Duration = Duration::from_secs(5);

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Persisted session for restore across restarts.
#[derive(Serialize, Deserialize)]
struct SavedSession {
    user_id: String,
    device_id: String,
    access_token: String,
}

/// Metadata about an attachment in a Matrix message.
#[derive(Debug, Clone)]
pub struct AttachmentMeta {
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    pub mxc_uri: String,
}

/// A permission verdict from Matrix to be relayed back to Claude Code.
#[derive(Debug, Clone)]
pub struct PermissionVerdict {
    pub request_id: String,
    pub behavior: String, // "allow" or "deny"
}

/// Parse a permission verdict from a message like "yes abcde" or "no abcde".
/// Strips Matrix reply fallback before parsing.
/// Returns None if the message doesn't match the expected format.
pub fn parse_permission_verdict(text: &str) -> Option<PermissionVerdict> {
    use matrix_sdk::ruma::events::room::message::sanitize;
    let text = sanitize::remove_plain_reply_fallback(text).trim();
    let (word, rest) = text.split_once(|c: char| c.is_whitespace())?;
    let behavior = match word.to_lowercase().as_str() {
        "y" | "yes" => "allow",
        "n" | "no" => "deny",
        _ => return None,
    };
    let id = rest.trim();
    // Must be exactly 5 lowercase letters from a-z minus 'l'
    if id.len() != 5 || !id.chars().all(|c| c.is_ascii_lowercase() && c != 'l') {
        return None;
    }
    Some(PermissionVerdict {
        request_id: id.to_string(),
        behavior: behavior.to_string(),
    })
}

/// Reaction keycap emoji used to answer a pending prompt by index — `1️⃣` selects option 0,
/// `2️⃣` option 1, and so on. Length matches `crate::pending_prompt::MAX_REACTION_OPTIONS`,
/// which is what bounds how many options a prompt can offer before it stops being
/// reaction-answerable in the first place.
pub(crate) const NUMBER_EMOJI: [&str; crate::pending_prompt::MAX_REACTION_OPTIONS] =
    ["1️⃣", "2️⃣", "3️⃣", "4️⃣", "5️⃣", "6️⃣", "7️⃣", "8️⃣", "9️⃣"];

/// Map a reaction's emoji key to a zero-based option index. `None` for anything that isn't
/// one of the nine numbered keycaps — including the bridge's own outbound ✅/❌ ack
/// reactions, which must never be mistaken for an answer.
pub(crate) fn emoji_to_option_index(key: &str) -> Option<usize> {
    NUMBER_EMOJI.iter().position(|e| *e == key)
}

/// Reaction that declines a pending prompt outright — see
/// `PendingPrompt::decline_option_index` for what it actually sends. Distinct from the
/// bridge's own outbound ✅/❌ permission-verdict acks: those come from the bridge's own
/// account, which `handle_reaction` already ignores before any emoji is even looked at.
pub(crate) const DECLINE_EMOJI: &str = "❌";

/// Reaction that declines an `AskUserQuestion` prompt but asks Claude to clarify instead of
/// stopping silently — see `PendingPrompt::chat_option_index` for what it actually sends.
/// Never offered for `ExitPlanMode` (that method returns `None` for it), so this can never
/// collide with `DECLINE_EMOJI`'s meaning there. A question mark rather than a speech
/// bubble — Sky's call, matches what the reaction actually does (asks a clarifying
/// question) better than a generic "chat" icon.
pub(crate) const CHAT_EMOJI: &str = "❓";

/// A menu prompt currently open in the room, waiting for a reaction answer.
///
/// Lives here rather than in `pending_prompt` because it's Matrix-side bookkeeping (an
/// event id and which room it's in), not transcript-derived fact — the same split
/// `PermissionVerdict`/`pending_permissions` already draws between "what Claude Code asked"
/// and "what the bridge is tracking about it."
#[derive(Debug, Clone)]
pub struct PendingAnswer {
    pub tool_use_id: String,
    pub kind: crate::pending_prompt::PromptKind,
    pub option_count: usize,
    /// Carried straight from `PendingPrompt::decline_option_index` at post time, so
    /// `reaction_claims_answer` never has to reconstruct it (or reach back into
    /// `pending_prompt` at all) from just an emoji and a room. See that method's doc for
    /// why this is a different index than any numbered reaction, for both prompt kinds.
    pub decline_option_index: usize,
    /// Carried straight from `PendingPrompt::chat_option_index` at post time, same
    /// reasoning as `decline_option_index`. `None` for `ExitPlanMode`, which has no
    /// equivalent option — `reaction_claims_answer` treats ❓ as not offered at all in
    /// that case, the same as any other emoji this prompt doesn't recognize.
    pub chat_option_index: Option<usize>,
    pub room_id: OwnedRoomId,
}

/// A resolved answer to relay into the tmux pane, produced by [`MatrixBridge::handle_reaction`]
/// / [`MatrixBridge::handle_message`]'s reply-feedback interception, and consumed by the
/// tmux-relay task in `main.rs`.
#[derive(Debug, Clone)]
pub struct MenuAnswer {
    pub tool_use_id: String,
    pub kind: crate::pending_prompt::PromptKind,
    pub option_index: usize,
    /// Carried straight from the [`PendingAnswer`] this claimed, so the tmux relay's
    /// pre-send pane-shape check has the real validated count to check against — not just
    /// a lower bound inferred from which index was picked.
    pub option_count: usize,
    /// Set only by the reply-with-text path — `handle_message`'s reply interception,
    /// never `handle_reaction`. `Some(text)` tells
    /// `tmux_relay::TmuxRelay::answer_prompt` to type `text` into the free-text option
    /// (`PendingAnswer::decline_option_index`) rather than submitting it blank
    /// (`None`, a plain numbered or decline answer, which never types anything). What
    /// happens next differs by kind, both confirmed live:
    /// - `ExitPlanMode`: submits with `shift+tab` — approves the plan *and* attaches
    ///   `text` as feedback in the same turn.
    /// - `AskUserQuestion`: submits with plain `Enter` — `text` *is* the answer, no
    ///   separate approve step.
    pub feedback: Option<String>,
}

/// Why a reaction did not claim a [`PendingAnswer`] — see [`reaction_claims_answer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReactionRejection {
    /// The reaction's own room doesn't match the room the prompt was posted in — see
    /// [`reaction_claims_answer`]'s doc for why this matters. The one case
    /// `MatrixBridge::handle_reaction` logs, since it's a genuine mismatch worth knowing
    /// about rather than ordinary noise.
    WrongRoom,
    /// `AccessControl::check_sender` rejected the sender.
    AccessDenied,
    /// The reaction's emoji isn't one of the nine numbered keycaps.
    NotANumberedReaction,
    /// A numbered keycap, but past `answer.option_count` (e.g. reacting 4️⃣ on a 3-option
    /// prompt, or on `ExitPlanMode`'s free-text option once excluded from
    /// `option_count` — see `PendingPrompt::reaction_option_count`).
    OptionOutOfRange,
}

/// Pure decision: does this reaction claim `answer`? Kept separate from the async
/// Matrix/access-control calls so the security-relevant logic (room match, access result,
/// emoji mapping, bounds check) is unit-testable without a homeserver — the same split
/// `live_status::menu_action` already uses elsewhere in this codebase.
///
/// `access_ok` is passed in already-evaluated rather than computed here:
/// `AccessControl::check_sender` needs a live `&AccessControl` and Matrix sender/room
/// types this function has no business depending on, the same reason `menu_action` takes
/// plain `&str` ids rather than owned Matrix types.
///
/// The room check exists because `pending_answers` is keyed by Matrix event id alone,
/// which is globally unique — but a reaction event only carries the id it relates to, not
/// which room *that* event lives in. Without it, a member of a *different* room the
/// bridge talks to could react to a copy of the same event id (relayed, quoted, or simply
/// guessed) and have it treated as an answer to a prompt posted somewhere else entirely.
/// Caught in code review as a real gap, not a hypothetical.
fn reaction_claims_answer(
    reaction_room: &OwnedRoomId,
    answer: &PendingAnswer,
    access_ok: bool,
    emoji: &str,
) -> Result<usize, ReactionRejection> {
    if reaction_room != &answer.room_id {
        return Err(ReactionRejection::WrongRoom);
    }
    if !access_ok {
        return Err(ReactionRejection::AccessDenied);
    }
    // Decline is checked before the numbered lookup and skips the `option_count` bounds
    // check entirely: `answer.decline_option_index` is deliberately outside that range for
    // `AskUserQuestion` (see `PendingPrompt::decline_option_index`'s doc — it targets the
    // CLI's own fixed reject entry, one past the last real option), so bounding it against
    // `option_count` the way a numbered reaction is would reject every legitimate decline.
    if emoji == DECLINE_EMOJI {
        return Ok(answer.decline_option_index);
    }
    // Same reasoning as decline, and same bounds-skip: `chat_option_index` sits one past
    // even the decline index. `None` (no such option for this prompt — `ExitPlanMode`
    // always) is treated as "not a recognized reaction here," the same as any emoji this
    // prompt simply doesn't offer.
    if emoji == CHAT_EMOJI {
        return answer
            .chat_option_index
            .ok_or(ReactionRejection::NotANumberedReaction);
    }
    let option_index =
        emoji_to_option_index(emoji).ok_or(ReactionRejection::NotANumberedReaction)?;
    if option_index >= answer.option_count {
        return Err(ReactionRejection::OptionOutOfRange);
    }
    Ok(option_index)
}

/// A notification from Matrix to be forwarded to Claude Code via MCP.
#[derive(Debug, Clone)]
pub struct ChannelNotification {
    pub content: String,
    pub sender: String,
    pub sender_display_name: String,
    pub room_id: String,
    pub event_id: String,
    pub timestamp: String,
    pub attachments: Vec<AttachmentMeta>,
}

/// Configuration for constructing a [`MatrixBridge`].
pub struct MatrixBridgeConfig {
    pub notification_tx: mpsc::Sender<ChannelNotification>,
    pub permission_verdict_tx: mpsc::Sender<PermissionVerdict>,
    pub access_control: Arc<AccessControl>,
    pub known_rooms: Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>>,
    /// Room that most recently passed the access check — where live status is posted.
    pub last_active_room: Arc<parking_lot::Mutex<Option<OwnedRoomId>>>,
    pub pending_permissions: Arc<parking_lot::Mutex<HashSet<String>>>,
    /// Menu prompts currently open in the room, keyed by the Matrix event id of the
    /// message showing them — populated by `live_status.rs` when it posts one, consumed
    /// (single-shot, remove-on-claim) by [`MatrixBridge::handle_reaction`].
    pub pending_answers: Arc<parking_lot::Mutex<HashMap<OwnedEventId, PendingAnswer>>>,
    /// Where a claimed reaction answer goes to be relayed into the tmux pane.
    pub menu_answer_tx: mpsc::Sender<MenuAnswer>,
    pub cancel: CancellationToken,
}

/// Shared state captured by the Matrix event handler closure.
#[derive(Clone)]
struct MessageHandlerCtx {
    tx: mpsc::Sender<ChannelNotification>,
    permission_verdict_tx: mpsc::Sender<PermissionVerdict>,
    pending_permissions: Arc<parking_lot::Mutex<HashSet<String>>>,
    pending_answers: Arc<parking_lot::Mutex<HashMap<OwnedEventId, PendingAnswer>>>,
    menu_answer_tx: mpsc::Sender<MenuAnswer>,
    /// Mirrors `Config::tmux_answers_enabled`. `handle_reaction`'s ack reaction reads this
    /// to avoid a ✅ that lies: with the kill switch off (the default), a claimed reaction
    /// is enqueued but never actually applied to the terminal — acking success anyway would
    /// look identical, from the room, to a real answer landing. Caught in code review.
    tmux_answers_enabled: bool,
    access: Arc<AccessControl>,
    own_user_id: OwnedUserId,
    known_rooms: Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>>,
    last_active_room: Arc<parking_lot::Mutex<Option<OwnedRoomId>>>,
    start_time: Instant,
}

/// Why the supervised sync task stopped running.
enum SyncOutcome {
    /// Shutdown was requested.
    Cancelled,
    /// The sync loop returned on its own (cleanly, with an error, or by panic).
    Returned(std::result::Result<matrix_sdk::Result<()>, tokio::task::JoinError>),
    /// The watchdog saw no completed sync for this many seconds.
    Stalled(u64),
}

pub struct MatrixBridge {
    client: Client,
    own_user_id: OwnedUserId,
    notification_tx: mpsc::Sender<ChannelNotification>,
    permission_verdict_tx: mpsc::Sender<PermissionVerdict>,
    access_control: Arc<AccessControl>,
    known_rooms: Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>>,
    last_active_room: Arc<parking_lot::Mutex<Option<OwnedRoomId>>>,
    pending_permissions: Arc<parking_lot::Mutex<HashSet<String>>>,
    pending_answers: Arc<parking_lot::Mutex<HashMap<OwnedEventId, PendingAnswer>>>,
    menu_answer_tx: mpsc::Sender<MenuAnswer>,
    /// Read from `Config` (not `MatrixBridgeConfig` — no shared-state plumbing needed,
    /// `Config` is already a direct parameter here) and threaded into
    /// `MessageHandlerCtx` so `handle_reaction` knows whether an ack it's about to send
    /// would actually be true. See `MessageHandlerCtx::tmux_answers_enabled`'s doc.
    tmux_answers_enabled: bool,
    start_time: Instant,
    cancel: CancellationToken,
}

impl MatrixBridge {
    pub async fn new(config: &Config, bridge_config: MatrixBridgeConfig) -> Result<Self> {
        let MatrixBridgeConfig {
            notification_tx,
            permission_verdict_tx,
            access_control,
            known_rooms,
            last_active_room,
            pending_permissions,
            pending_answers,
            menu_answer_tx,
            cancel,
        } = bridge_config;
        let tmux_answers_enabled = config.tmux_answers_enabled;
        tokio::fs::create_dir_all(&config.store_path).await?;

        let homeserver_url = config
            .homeserver_url
            .as_ref()
            .context("MATRIX_HOMESERVER_URL is required")?;

        // Build client with E2EE settings.
        //
        // `retry_timeout` is set explicitly: without it the SDK retries transient
        // (5xx / 429) failures with unbounded exponential backoff and the request
        // future never resolves. See REQUEST_RETRY_TIMEOUT.
        let client = Client::builder()
            .homeserver_url(homeserver_url)
            .request_config(RequestConfig::default().retry_timeout(REQUEST_RETRY_TIMEOUT))
            .sqlite_store(&config.store_path, config.store_passphrase.as_deref())
            .with_encryption_settings(matrix_sdk::encryption::EncryptionSettings {
                auto_enable_cross_signing: true,
                auto_enable_backups: true,
                backup_download_strategy:
                    matrix_sdk::encryption::BackupDownloadStrategy::AfterDecryptionFailure,
            })
            .build()
            .await
            .context("Failed to build Matrix client")?;

        let session_file = session_file_path(&config.store_path);
        let user_id_str = config
            .user_id
            .as_ref()
            .context("MATRIX_USER_ID is required")?;
        let user_id = OwnedUserId::try_from(user_id_str.as_str())
            .context(format!("Invalid MATRIX_USER_ID: {user_id_str}"))?;

        // Three login paths: saved session > password login > access token fallback
        if session_file.exists() {
            // Path 1: Restore from saved session
            tracing::info!("Restoring session from {}", session_file.display());
            let saved = load_session(&session_file).await?;
            let session = matrix_sdk::matrix_auth::MatrixSession {
                meta: matrix_sdk::SessionMeta {
                    user_id: OwnedUserId::try_from(saved.user_id.as_str())?,
                    device_id: OwnedDeviceId::from(saved.device_id.as_str()),
                },
                tokens: matrix_sdk::matrix_auth::MatrixSessionTokens {
                    access_token: saved.access_token,
                    refresh_token: None,
                },
            };
            client
                .matrix_auth()
                .restore_session(session)
                .await
                .context("Failed to restore saved session")?;
            let resp = client
                .whoami()
                .await
                .context("Session invalid — delete the store and re-login")?;
            tracing::info!(
                "Session restored: {} (device {})",
                resp.user_id,
                saved.device_id
            );
        } else if let Some(ref password) = config.password {
            // Path 2: First-run login with password
            tracing::info!("First-run login for {}", user_id);
            let localpart = config
                .user_localpart()
                .context("MATRIX_USER_ID is required for login")?;

            let mut login_builder = client.matrix_auth().login_username(localpart, password);

            // Use configured device_id if provided, otherwise SDK generates one
            if let Some(ref device_id) = config.device_id {
                login_builder = login_builder.device_id(device_id);
            }

            login_builder
                .initial_device_display_name("cc_matrix_channel")
                .send()
                .await
                .context("Login failed — check username/password")?;

            // Wait for E2EE initialization (cross-signing bootstrap, key upload)
            tracing::info!("Waiting for E2EE initialization...");
            client
                .encryption()
                .wait_for_e2ee_initialization_tasks()
                .await;
            tracing::info!(
                "E2EE initialization complete — cross-signing bootstrapped, device keys uploaded"
            );

            // Save session for future restarts
            let session = client
                .matrix_auth()
                .session()
                .context("No session after login")?;
            let saved = SavedSession {
                user_id: session.meta.user_id.to_string(),
                device_id: session.meta.device_id.to_string(),
                access_token: session.tokens.access_token.clone(),
            };
            save_session(&session_file, &saved).await?;
            tracing::info!(
                "Session saved to {} (device {})",
                session_file.display(),
                saved.device_id
            );
        } else if let Some(ref access_token) = config.access_token {
            // Path 3: Access token fallback (no password, no saved session)
            tracing::warn!(
                "Using access token without password login — E2EE may not work for encrypted media. \
                 Set MATRIX_PASSWORD for full E2EE support."
            );
            let device_id = config.device_id.as_deref().unwrap_or("cc_matrix_channel");
            let session = matrix_sdk::matrix_auth::MatrixSession {
                meta: matrix_sdk::SessionMeta {
                    user_id: user_id.clone(),
                    device_id: OwnedDeviceId::from(device_id),
                },
                tokens: matrix_sdk::matrix_auth::MatrixSessionTokens {
                    access_token: access_token.clone(),
                    refresh_token: None,
                },
            };
            client
                .matrix_auth()
                .restore_session(session)
                .await
                .context("Failed to restore session from access token")?;
            client
                .whoami()
                .await
                .context("whoami failed — is the access token valid?")?;
        } else {
            bail!(
                "No authentication configured. Set MATRIX_PASSWORD for first-run E2EE setup, \
                 or MATRIX_ACCESS_TOKEN for token-based auth (limited E2EE)."
            );
        }

        let own_user_id = client
            .user_id()
            .context("No user_id after login")?
            .to_owned();
        tracing::info!("Bot identity: {own_user_id}");

        Ok(Self {
            client,
            own_user_id,
            notification_tx,
            permission_verdict_tx,
            access_control,
            pending_permissions,
            pending_answers,
            menu_answer_tx,
            tmux_answers_enabled,
            known_rooms,
            last_active_room,
            start_time: Instant::now(),
            cancel,
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub async fn run(&self) -> Result<()> {
        self.client.add_event_handler(Self::handle_invite);

        let ctx = MessageHandlerCtx {
            tx: self.notification_tx.clone(),
            permission_verdict_tx: self.permission_verdict_tx.clone(),
            pending_permissions: self.pending_permissions.clone(),
            pending_answers: self.pending_answers.clone(),
            menu_answer_tx: self.menu_answer_tx.clone(),
            tmux_answers_enabled: self.tmux_answers_enabled,
            access: self.access_control.clone(),
            own_user_id: self.own_user_id.clone(),
            known_rooms: self.known_rooms.clone(),
            last_active_room: self.last_active_room.clone(),
            start_time: self.start_time,
        };

        // Use Raw<AnySyncTimelineEvent> for manual deserialization — more robust than
        // typed OriginalSyncRoomMessageEvent which silently skips on deserialization failure
        // (known issue with encrypted media events in matrix-sdk v0.9)
        self.client
            .add_event_handler(move |raw: Raw<AnySyncTimelineEvent>, room: Room| {
                let ctx = ctx.clone();
                async move {
                    match raw.deserialize() {
                        Ok(AnySyncTimelineEvent::MessageLike(
                            AnySyncMessageLikeEvent::RoomMessage(msg),
                        )) => {
                            if let Some(original) = msg.as_original() {
                                Self::handle_message(original.clone(), room, ctx).await;
                            }
                        }
                        Ok(AnySyncTimelineEvent::MessageLike(
                            AnySyncMessageLikeEvent::Reaction(reaction),
                        )) => {
                            if let Some(original) = reaction.as_original() {
                                Self::handle_reaction(original.clone(), room, ctx).await;
                            }
                        }
                        Ok(_) => {} // other timeline events
                        Err(e) => {
                            tracing::debug!("Timeline event deserialization skipped: {e}");
                        }
                    }
                }
            });

        tracing::info!(
            "Starting Matrix sync loop (stall watchdog: {}s)",
            SYNC_STALL_TIMEOUT.as_secs()
        );
        self.supervise_sync().await
    }

    /// Runs the sync loop under a liveness watchdog, restarting it if it stalls
    /// or fails.
    ///
    /// The sync loop runs in its own task specifically so a *wedged* sync can be
    /// aborted. A wedged sync returns no error and exits no loop — the process
    /// stays up and the MCP server keeps answering, so from the outside the
    /// bridge looks healthy while never delivering another message. The only
    /// reliable signal is the absence of completed syncs, which is what the
    /// heartbeat below measures.
    async fn supervise_sync(&self) -> Result<()> {
        let heartbeat = Arc::new(AtomicU64::new(now_unix()));
        let mut consecutive_failures: u32 = 0;

        loop {
            if self.cancel.is_cancelled() {
                break;
            }

            heartbeat.store(now_unix(), Ordering::Relaxed);

            let client = self.client.clone();
            let cancel = self.cancel.clone();
            let beat = heartbeat.clone();
            let mut sync_task = tokio::spawn(async move {
                client
                    .sync_with_callback(SyncSettings::default(), move |_response| {
                        let cancel = cancel.clone();
                        let beat = beat.clone();
                        async move {
                            beat.store(now_unix(), Ordering::Relaxed);
                            if cancel.is_cancelled() {
                                LoopCtrl::Break
                            } else {
                                LoopCtrl::Continue
                            }
                        }
                    })
                    .await
            });

            let outcome = loop {
                tokio::select! {
                    joined = &mut sync_task => break SyncOutcome::Returned(joined),
                    _ = self.cancel.cancelled() => {
                        sync_task.abort();
                        break SyncOutcome::Cancelled;
                    }
                    _ = tokio::time::sleep(WATCHDOG_TICK) => {
                        let idle = now_unix().saturating_sub(heartbeat.load(Ordering::Relaxed));
                        if idle >= SYNC_STALL_TIMEOUT.as_secs() {
                            sync_task.abort();
                            break SyncOutcome::Stalled(idle);
                        }
                    }
                }
            };

            match outcome {
                SyncOutcome::Cancelled => break,
                SyncOutcome::Returned(Ok(Ok(()))) => {
                    tracing::info!("Matrix sync loop stopped");
                    break;
                }
                SyncOutcome::Stalled(idle) => {
                    consecutive_failures += 1;
                    tracing::error!(
                        "Matrix sync stalled — no sync completed for {idle}s (failure #{consecutive_failures}); restarting sync loop"
                    );
                }
                SyncOutcome::Returned(Ok(Err(e))) => {
                    consecutive_failures += 1;
                    tracing::error!(
                        "Matrix sync loop failed (failure #{consecutive_failures}): {e:?}; restarting"
                    );
                }
                SyncOutcome::Returned(Err(e)) => {
                    consecutive_failures += 1;
                    tracing::error!(
                        "Matrix sync task terminated unexpectedly (failure #{consecutive_failures}): {e}; restarting"
                    );
                }
            }

            // Escalating backoff so a hard, permanent failure (bad token, for
            // instance) doesn't turn into a hot restart loop.
            let backoff = SYNC_RESTART_DELAY * consecutive_failures.min(12);
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = self.cancel.cancelled() => break,
            }
        }

        Ok(())
    }

    async fn handle_invite(event: StrippedRoomMemberEvent, room: Room) {
        let client = room.client();
        let own_id = client.user_id();
        if own_id.is_some_and(|id| *id == *event.state_key) {
            let room_id = room.room_id().to_owned();
            tracing::info!("Received invite to room {room_id}, joining");
            // Detached: joining is a network round trip and must not run on the
            // sync loop (see spawn_room_send).
            tokio::spawn(async move {
                match tokio::time::timeout(SIDE_EFFECT_TIMEOUT, room.join()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::error!("Failed to join room {room_id}: {e}"),
                    Err(_) => tracing::error!("Timed out joining room {room_id}"),
                }
            });
        }
    }

    async fn handle_message(
        event: OriginalSyncRoomMessageEvent,
        room: Room,
        ctx: MessageHandlerCtx,
    ) {
        let MessageHandlerCtx {
            tx,
            permission_verdict_tx,
            pending_permissions,
            access,
            own_user_id,
            known_rooms,
            last_active_room,
            start_time,
            // Used below by the reply-feedback interception — a reply to a still-pending
            // `ExitPlanMode` prompt claims it the same way a reaction does, just with typed
            // text attached. `handle_reaction` is the *other* consumer of these same three.
            pending_answers,
            menu_answer_tx,
            tmux_answers_enabled,
        } = ctx;

        if event.sender == own_user_id {
            return;
        }

        // Log message type for debugging
        tracing::debug!(
            "Received message from {} in {}: type={:?}",
            event.sender,
            room.room_id(),
            std::mem::discriminant(&event.content.msgtype)
        );

        let (text, attachments) = match &event.content.msgtype {
            MessageType::Text(t) => {
                use matrix_sdk::ruma::events::room::message::sanitize;
                let body = sanitize::remove_plain_reply_fallback(&t.body).to_string();
                (body, vec![])
            }
            MessageType::Image(img) => {
                let meta = AttachmentMeta {
                    name: img.body.clone(),
                    mime_type: img
                        .info
                        .as_ref()
                        .and_then(|i| i.mimetype.as_deref())
                        .unwrap_or("image/unknown")
                        .to_string(),
                    size: img
                        .info
                        .as_ref()
                        .and_then(|i| i.size)
                        .map(u64::from)
                        .unwrap_or(0),
                    mxc_uri: extract_mxc_uri(&img.source),
                };
                (format!("[Image: {}]", img.body), vec![meta])
            }
            MessageType::File(file) => {
                let meta = AttachmentMeta {
                    name: file.body.clone(),
                    mime_type: file
                        .info
                        .as_ref()
                        .and_then(|i| i.mimetype.as_deref())
                        .unwrap_or("application/octet-stream")
                        .to_string(),
                    size: file
                        .info
                        .as_ref()
                        .and_then(|i| i.size)
                        .map(u64::from)
                        .unwrap_or(0),
                    mxc_uri: extract_mxc_uri(&file.source),
                };
                (format!("[File: {}]", file.body), vec![meta])
            }
            MessageType::Audio(audio) => {
                let meta = AttachmentMeta {
                    name: audio.body.clone(),
                    mime_type: audio
                        .info
                        .as_ref()
                        .and_then(|i| i.mimetype.as_deref())
                        .unwrap_or("audio/unknown")
                        .to_string(),
                    size: audio
                        .info
                        .as_ref()
                        .and_then(|i| i.size)
                        .map(u64::from)
                        .unwrap_or(0),
                    mxc_uri: extract_mxc_uri(&audio.source),
                };
                (format!("[Audio: {}]", audio.body), vec![meta])
            }
            MessageType::Video(video) => {
                let meta = AttachmentMeta {
                    name: video.body.clone(),
                    mime_type: video
                        .info
                        .as_ref()
                        .and_then(|i| i.mimetype.as_deref())
                        .unwrap_or("video/unknown")
                        .to_string(),
                    size: video
                        .info
                        .as_ref()
                        .and_then(|i| i.size)
                        .map(u64::from)
                        .unwrap_or(0),
                    mxc_uri: extract_mxc_uri(&video.source),
                };
                (format!("[Video: {}]", video.body), vec![meta])
            }
            _ => return,
        };

        // Which event (if any) this message is a Matrix reply to — used below by the
        // reply-feedback interception. `Relation::Reply` is ruma's dedicated reply variant
        // (distinct from `Annotation`, which is reactions' own relation type); anything
        // else (a thread reply, an edit, no relation at all) is `None` here, same as an
        // ordinary top-level message.
        let reply_target = match &event.content.relates_to {
            Some(Relation::Reply { in_reply_to }) => Some(in_reply_to.event_id.clone()),
            _ => None,
        };

        // Handle bot commands before access check
        if text.starts_with('/') {
            Self::handle_bot_command(&text, &room, &own_user_id, start_time).await;
            return;
        }

        let sender_id = event.sender.clone();

        // Mention-only room check (from access.json groups config)
        if access.requires_mention(room.room_id()) {
            let own_id_str = own_user_id.as_str();
            if !text.contains(own_id_str) {
                // Bounded: this runs on the sync loop, so it must not be able to
                // hang (see spawn_room_send).
                let own_name = match tokio::time::timeout(
                    SIDE_EFFECT_TIMEOUT,
                    room.client().account().get_display_name(),
                )
                .await
                {
                    Ok(Ok(name)) => name,
                    Ok(Err(e)) => {
                        tracing::warn!("Failed to fetch own display name: {e}");
                        None
                    }
                    Err(_) => {
                        tracing::warn!("Timed out fetching own display name");
                        None
                    }
                };
                let mentioned = own_name
                    .as_ref()
                    .is_some_and(|name| text.contains(name.as_str()));
                if !mentioned {
                    // Check custom mention patterns from config (regex, case-insensitive)
                    let patterns = access.mention_patterns(room.room_id());
                    let pattern_matched =
                        patterns.iter().any(|pat| {
                            match regex::RegexBuilder::new(pat).case_insensitive(true).build() {
                                Ok(re) => re.is_match(&text),
                                Err(e) => {
                                    tracing::warn!("Invalid mention pattern '{pat}': {e}");
                                    false
                                }
                            }
                        });
                    if !pattern_matched {
                        return;
                    }
                }
            }
        }

        // Check access
        let current_room_id = room.room_id().to_owned();
        match access.check_sender(&sender_id, &current_room_id) {
            Ok(()) => {
                // Remember this room, and remember it is the most recent one — live status
                // posts there. Persisted so a bridge restart does not go silent.
                let newly_known = known_rooms.lock().insert(current_room_id.clone());
                let room_changed = {
                    let mut last = last_active_room.lock();
                    let changed = last.as_ref() != Some(&current_room_id);
                    *last = Some(current_room_id.clone());
                    changed
                };
                if newly_known || room_changed {
                    let rooms = known_rooms.lock().clone();
                    crate::rooms::save(&crate::rooms::store_path(), &rooms, Some(&current_room_id));
                }

                // Reply-with-text interception — a reply to a still-pending prompt claims
                // it the same way a reaction does, just with typed text attached instead
                // of a numbered choice (confirmed live for both kinds — see
                // `tools/menu-spike/FINDINGS.md`'s options-4/5/3 section):
                // - `ExitPlanMode`: approves the plan and attaches the reply text as
                //   feedback in the same turn, the outcome `shift+tab` produces at the
                //   terminal.
                // - `AskUserQuestion`: the reply text *is* the answer — the same fixed
                //   "Type something." option that declines when submitted blank captures
                //   whatever's typed into it as the model's real answer.
                // Both reuse `answer.decline_option_index` as the target: it's the same
                // free-text box either way, just a different final keystroke
                // (`tmux_relay::TmuxRelay::answer_prompt` decides that part).
                if let Some(target) = reply_target.clone() {
                    let claimed = pending_answers.lock().get(&target).cloned();
                    if let Some(answer) = claimed
                        // Single-shot claim, same idiom `handle_reaction` uses: only the
                        // reply that actually removes the entry proceeds.
                        && pending_answers.lock().remove(&target).is_some()
                    {
                        tracing::info!(
                            tool_use_id = %answer.tool_use_id,
                            "Menu feedback claimed via reply"
                        );
                        let menu_answer = MenuAnswer {
                            tool_use_id: answer.tool_use_id,
                            kind: answer.kind,
                            option_index: answer.decline_option_index,
                            option_count: answer.option_count,
                            feedback: Some(text.clone()),
                        };
                        if menu_answer_tx.send(menu_answer).await.is_err() {
                            tracing::error!(
                                "Menu-answer relay task is gone; feedback claimed but not delivered"
                            );
                        }
                        // Same conditional-ack posture as `handle_reaction`'s: 👀 when the
                        // kill switch would drop the keystrokes anyway, ✅ only when they
                        // might actually reach the terminal.
                        let emoji = if tmux_answers_enabled { "✅" } else { "👀" };
                        let annotation = Annotation::new(event.event_id.clone(), emoji.to_string());
                        let reaction = ReactionEventContent::new(annotation);
                        spawn_room_send("menu-feedback ack", room.clone(), reaction);
                        return;
                    }
                }

                // Permission verdict interception — only for pending requests from approved users
                if let Some(verdict) = parse_permission_verdict(&text)
                    && pending_permissions.lock().contains(&verdict.request_id)
                {
                    let _ = permission_verdict_tx.send(verdict.clone()).await;
                    let emoji = if verdict.behavior == "allow" {
                        "✅"
                    } else {
                        "❌"
                    };
                    let annotation = matrix_sdk::ruma::events::relation::Annotation::new(
                        event.event_id.clone(),
                        emoji.to_string(),
                    );
                    let reaction =
                        matrix_sdk::ruma::events::reaction::ReactionEventContent::new(annotation);
                    spawn_room_send("verdict reaction", room.clone(), reaction);
                    return;
                }

                // Typing indicator — only for text messages (media won't get a Claude response)
                if attachments.is_empty() {
                    let typing_room = room.clone();
                    let typing_room_id = room.room_id().to_owned();
                    tokio::spawn(async move {
                        match tokio::time::timeout(
                            SIDE_EFFECT_TIMEOUT,
                            typing_room.typing_notice(true),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                tracing::warn!(
                                    "Failed to send typing notice in {typing_room_id}: {e}"
                                )
                            }
                            Err(_) => {
                                tracing::warn!(
                                    "Timed out sending typing notice in {typing_room_id}"
                                )
                            }
                        }
                    });
                }

                let display_name = room
                    .get_member_no_sync(&sender_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|m| m.name().to_string())
                    .unwrap_or_else(|| sender_id.to_string());

                let timestamp = event
                    .origin_server_ts
                    .to_system_time()
                    .map(humanize_timestamp)
                    .unwrap_or_default();

                let notif = ChannelNotification {
                    content: text,
                    sender: sender_id.to_string(),
                    sender_display_name: display_name,
                    room_id: room.room_id().to_string(),
                    event_id: event.event_id.to_string(),
                    timestamp,
                    attachments,
                };
                // Bounded: a stalled MCP consumer must not be able to wedge sync.
                let queued = match tokio::time::timeout(NOTIFY_TIMEOUT, tx.send(notif)).await {
                    Ok(Ok(())) => true,
                    Ok(Err(_)) => {
                        tracing::error!("Failed to send notification to MCP");
                        false
                    }
                    Err(_) => {
                        tracing::error!(
                            "Timed out after {}s queueing notification to MCP — consumer stalled",
                            NOTIFY_TIMEOUT.as_secs()
                        );
                        false
                    }
                };

                if queued {
                    // Ack reaction — confirms message was received
                    if let Some(emoji) = access.ack_reaction() {
                        let annotation = matrix_sdk::ruma::events::relation::Annotation::new(
                            event.event_id.clone(),
                            emoji,
                        );
                        let reaction =
                            matrix_sdk::ruma::events::reaction::ReactionEventContent::new(
                                annotation,
                            );
                        spawn_room_send("ack reaction", room.clone(), reaction);
                    }
                }
            }
            Err(AccessDenied::PairingRequired(code)) => {
                let msg = format!(
                    "Pairing required. Ask the Claude Code operator to approve you with code: {code}"
                );
                let content = RoomMessageEventContent::text_plain(&msg);
                spawn_room_send("pairing message", room.clone(), content);
                access.mark_pairing_reply_sent(&sender_id);
            }
            Err(
                AccessDenied::PairingPending(_)
                | AccessDenied::TooManyPending
                | AccessDenied::Denied,
            ) => {
                // Silent drop
            }
        }
    }

    /// Answer a pending menu prompt by reaction — the counterpart to `handle_message`'s
    /// permission-verdict interception, but for `AskUserQuestion`/`ExitPlanMode` instead of
    /// tool-permission prompts.
    ///
    /// Gated by the same [`AccessControl::check_sender`] every ordinary message goes
    /// through (confirmed as the intended v1 posture, not a placeholder — see the plan's
    /// "Access control" section). Single-shot: [`Arc<Mutex<HashMap>>::remove`] on claim, the
    /// same idiom `pending_permissions` already uses, so a second reaction (or two people
    /// reacting near-simultaneously) can't both claim the same prompt.
    async fn handle_reaction(event: OriginalSyncReactionEvent, room: Room, ctx: MessageHandlerCtx) {
        if event.sender == ctx.own_user_id {
            // Our own ack (✅/👀) and permission-verdict (✅/❌) reactions land here too —
            // never mistake our own bookkeeping for an answer.
            return;
        }

        let target = event.content.relates_to.event_id.clone();

        // Look, don't yet claim: still need to check the room, the sender, and the emoji
        // before this reaction is allowed to consume the prompt.
        let Some(answer) = ctx.pending_answers.lock().get(&target).cloned() else {
            return; // not reacting to a message we're tracking as a pending prompt
        };

        let current_room_id = room.room_id().to_owned();
        let access_ok = ctx
            .access
            .check_sender(&event.sender, &current_room_id)
            .is_ok();

        let option_index = match reaction_claims_answer(
            &current_room_id,
            &answer,
            access_ok,
            &event.content.relates_to.key,
        ) {
            Ok(i) => i,
            Err(ReactionRejection::WrongRoom) => {
                tracing::warn!(
                    event_id = %target,
                    reaction_room = %current_room_id,
                    prompt_room = %answer.room_id,
                    "Reaction targets a pending prompt from a different room; ignoring"
                );
                return;
            }
            // Access denied, not a numbered keycap, or out of range — none warrant a log
            // line; these are ordinary noise (someone reacting with an unrelated emoji,
            // or a sender who just isn't paired), not the cross-room case above.
            Err(_) => return,
        };

        // Single-shot claim: only the reaction that actually removes the entry proceeds.
        if ctx.pending_answers.lock().remove(&target).is_none() {
            return;
        }

        tracing::info!(
            tool_use_id = %answer.tool_use_id,
            option_index,
            "Menu answer claimed via reaction"
        );

        let menu_answer = MenuAnswer {
            tool_use_id: answer.tool_use_id,
            kind: answer.kind,
            option_index,
            option_count: answer.option_count,
            // Numbered and decline reactions never carry typed text — that's the
            // reply-feedback path's job (`handle_message`), not this one's.
            feedback: None,
        };
        if ctx.menu_answer_tx.send(menu_answer).await.is_err() {
            tracing::error!("Menu-answer relay task is gone; answer claimed but not delivered");
        }

        // Ack reflects what will actually happen, not just "received" — caught in code
        // review: acking ✅ unconditionally, with the tmux kill switch off by default,
        // would tell the room "done" on every single reaction out of the box, while the
        // terminal stayed genuinely blocked. 👀 ("seen but not applied") when the switch
        // is off; ✅ only when an answer might actually reach the terminal.
        let emoji = if ctx.tmux_answers_enabled {
            "✅"
        } else {
            "👀"
        };
        let annotation = Annotation::new(target, emoji.to_string());
        let reaction = ReactionEventContent::new(annotation);
        spawn_room_send("menu-answer ack", room, reaction);
    }

    async fn handle_bot_command(
        text: &str,
        room: &Room,
        _own_user_id: &OwnedUserId,
        start_time: Instant,
    ) {
        let cmd = text.split_whitespace().next().unwrap_or("");
        let response = match cmd {
            "/start" => Some(
                "I'm a Claude Code bridge bot. Messages sent here are forwarded to your active Claude Code session, and Claude's replies appear back in this chat.".to_string()
            ),
            "/help" => Some(
                "Available commands:\n\
                 /start — What this bot does\n\
                 /help — Show this message\n\
                 /status — Bot status\n\n\
                 Send any other message and it will be forwarded to Claude Code (if you have access)."
                    .to_string(),
            ),
            "/status" => {
                let uptime = start_time.elapsed();
                let hours = uptime.as_secs() / 3600;
                let minutes = (uptime.as_secs() % 3600) / 60;
                // Answered by the bridge itself and never forwarded to Claude, so this
                // keeps working when the agent is the thing that is wedged.
                let agent = crate::status::read_status(crate::status::stall_threshold());
                Some(format!(
                    "Bridge:   online, uptime {hours}h {minutes}m\n{}",
                    agent.render()
                ))
            }
            _ => None,
        };

        if let Some(msg) = response {
            let content = RoomMessageEventContent::text_plain(&msg);
            spawn_room_send("bot command response", room.clone(), content);
        }
    }

    #[allow(dead_code)]
    pub async fn send_message(&self, room_id: &OwnedRoomId, text: &str) -> Result<()> {
        let room = self
            .client
            .get_room(room_id)
            .context(format!("Room not found: {room_id}"))?;
        let content = RoomMessageEventContent::text_markdown(text);
        room.send(content).await?;
        Ok(())
    }
}

// --- Outbound side effects ---

/// Send a room event without blocking the sync loop.
///
/// matrix-sdk awaits event-handler futures inline while processing a sync
/// response (`Client::call_event_handlers`), so anything awaited inside a
/// handler delays every subsequent message — and if it never returns, sync stops
/// forever with no error and no exit. Reactions, typing notices and courtesy
/// replies are all fire-and-forget, so they belong on their own task with a
/// hard timeout.
fn spawn_room_send<C>(label: &'static str, room: Room, content: C)
where
    C: matrix_sdk::ruma::events::MessageLikeEventContent + Send + 'static,
{
    let room_id = room.room_id().to_owned();
    tokio::spawn(async move {
        let send = async { room.send(content).await };
        match tokio::time::timeout(SIDE_EFFECT_TIMEOUT, send).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!("Failed to send {label} in {room_id}: {e}"),
            Err(_) => tracing::warn!("Timed out sending {label} in {room_id}"),
        }
    });
}

/// Send a reaction from the bridge's own account onto `event_id` — used by
/// `live_status.rs` to pre-seed the numbered keycap reactions on a freshly-posted
/// pending-prompt message. Confirmed in practice this matters, not just a nice-to-have:
/// without an existing reaction to tap, answering means opening the client's full emoji
/// picker and finding the right keycap by hand every time — tapping an *existing*
/// reaction (added by anyone, bot included) is the low-friction path most Matrix clients
/// support instead.
///
/// Called directly (awaited) from `live_status.rs`'s own tick-loop task, not the sync-loop
/// event handler — matches that file's existing `send_status`/`edit_status`, which do the
/// same. `spawn_room_send`'s fire-and-forget-with-timeout wrapping exists specifically to
/// protect the sync loop from blocking, which doesn't apply here.
pub async fn react(
    client: &Client,
    room_id: &OwnedRoomId,
    event_id: &OwnedEventId,
    emoji: &str,
) -> bool {
    let Some(room) = client.get_room(room_id) else {
        return false;
    };
    let annotation = Annotation::new(event_id.clone(), emoji.to_string());
    let content = ReactionEventContent::new(annotation);
    match room.send(content).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("Failed to pre-seed reaction {emoji} on {event_id}: {e}");
            false
        }
    }
}

// --- Session persistence ---

fn session_file_path(store_path: &str) -> PathBuf {
    PathBuf::from(store_path).join("session.json")
}

async fn load_session(path: &PathBuf) -> Result<SavedSession> {
    let data = tokio::fs::read_to_string(path)
        .await
        .context("Failed to read session file")?;
    serde_json::from_str(&data).context("Failed to parse session file")
}

async fn save_session(path: &PathBuf, session: &SavedSession) -> Result<()> {
    let data = serde_json::to_string_pretty(session)?;
    tokio::fs::write(path, &data)
        .await
        .context("Failed to write session file")?;
    // Restrict permissions — session contains access_token
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .await
            .context("Failed to set session file permissions")?;
    }
    Ok(())
}

// --- Helpers ---

pub fn extract_mxc_uri(source: &matrix_sdk::ruma::events::room::MediaSource) -> String {
    match source {
        matrix_sdk::ruma::events::room::MediaSource::Plain(uri) => uri.to_string(),
        matrix_sdk::ruma::events::room::MediaSource::Encrypted(encrypted) => {
            encrypted.url.to_string()
        }
    }
}

fn humanize_timestamp(time: std::time::SystemTime) -> String {
    let duration = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    let mut y = 1970i64;
    let mut remaining = days_since_epoch as i64;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days_in_months: [i64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    for (i, &dim) in days_in_months.iter().enumerate() {
        if remaining < dim {
            m = i + 1;
            break;
        }
        remaining -= dim;
    }
    let d = remaining + 1;

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_keycap_emoji_maps_to_its_zero_based_index() {
        for (i, emoji) in NUMBER_EMOJI.iter().enumerate() {
            assert_eq!(emoji_to_option_index(emoji), Some(i), "emoji: {emoji}");
        }
    }

    /// The bridge's own ack/verdict reactions (and anything else) must never be mistaken
    /// for a numbered answer.
    #[test]
    fn unrelated_emoji_is_not_an_option_index() {
        for emoji in ["✅", "❌", "👍", "🎉", ""] {
            assert_eq!(emoji_to_option_index(emoji), None, "emoji: {emoji}");
        }
    }

    // --- reaction_claims_answer ---

    fn room(id: &str) -> OwnedRoomId {
        OwnedRoomId::try_from(id).unwrap()
    }

    fn answer_in(room_id: &str, option_count: usize) -> PendingAnswer {
        PendingAnswer {
            tool_use_id: "toolu_test".to_string(),
            kind: crate::pending_prompt::PromptKind::AskUserQuestion,
            option_count,
            // Matches `PendingPrompt::decline_option_index`'s `AskUserQuestion` arm: one
            // past the last real option.
            decline_option_index: option_count,
            // Matches `PendingPrompt::chat_option_index`'s `AskUserQuestion` arm: one past
            // decline.
            chat_option_index: Some(option_count + 1),
            room_id: room(room_id),
        }
    }

    #[test]
    fn same_room_valid_emoji_in_range_claims_the_answer() {
        let a = answer_in("!room:example.com", 3);
        assert_eq!(
            reaction_claims_answer(&room("!room:example.com"), &a, true, "2️⃣"),
            Ok(1)
        );
    }

    /// The core security property this whole function exists for: a reaction from a
    /// *different* room than where the prompt was posted must never claim it, even with
    /// a valid emoji and a sender who'd otherwise pass access control.
    #[test]
    fn different_room_is_rejected_even_with_a_valid_emoji_and_access() {
        let a = answer_in("!room-a:example.com", 3);
        assert_eq!(
            reaction_claims_answer(&room("!room-b:example.com"), &a, true, "1️⃣"),
            Err(ReactionRejection::WrongRoom)
        );
    }

    #[test]
    fn access_denied_is_rejected_even_in_the_right_room() {
        let a = answer_in("!room:example.com", 3);
        assert_eq!(
            reaction_claims_answer(&room("!room:example.com"), &a, false, "1️⃣"),
            Err(ReactionRejection::AccessDenied)
        );
    }

    #[test]
    fn unrelated_emoji_is_rejected() {
        let a = answer_in("!room:example.com", 3);
        assert_eq!(
            reaction_claims_answer(&room("!room:example.com"), &a, true, "👍"),
            Err(ReactionRejection::NotANumberedReaction)
        );
    }

    /// Covers both an ordinary out-of-range reaction (4️⃣ on a 3-option prompt) and the
    /// `ExitPlanMode` free-text case once `option_count` is set from
    /// `reaction_option_count()` rather than the full option list — either way, this
    /// function doesn't need to know *why* the count is what it is, only that the index
    /// must stay under it.
    #[test]
    fn option_index_past_option_count_is_rejected() {
        let a = answer_in("!room:example.com", 2);
        assert_eq!(
            reaction_claims_answer(&room("!room:example.com"), &a, true, "3️⃣"),
            Err(ReactionRejection::OptionOutOfRange)
        );
    }

    /// The whole reason `decline_option_index` is checked before the numbered bounds
    /// check: for `AskUserQuestion` it deliberately sits *outside* `option_count` (see
    /// `PendingPrompt::decline_option_index`'s doc), so if ❌ went through the same
    /// `>= option_count` gate as a numbered reaction, every real decline would be rejected
    /// as `OptionOutOfRange`.
    #[test]
    fn decline_emoji_claims_the_answer_at_its_decline_index_even_past_option_count() {
        let a = answer_in("!room:example.com", 3);
        assert_eq!(a.decline_option_index, 3, "outside the 0..3 numbered range");
        assert_eq!(
            reaction_claims_answer(&room("!room:example.com"), &a, true, DECLINE_EMOJI),
            Ok(3)
        );
    }

    #[test]
    fn decline_still_checks_room_and_access_first() {
        let a = answer_in("!room-a:example.com", 3);
        assert_eq!(
            reaction_claims_answer(&room("!room-b:example.com"), &a, true, DECLINE_EMOJI),
            Err(ReactionRejection::WrongRoom)
        );
        assert_eq!(
            reaction_claims_answer(&room("!room-a:example.com"), &a, false, DECLINE_EMOJI),
            Err(ReactionRejection::AccessDenied)
        );
    }

    #[test]
    fn chat_emoji_claims_the_answer_at_its_own_index_past_decline() {
        let a = answer_in("!room:example.com", 3);
        assert_eq!(
            a.chat_option_index,
            Some(4),
            "one past decline_option_index"
        );
        assert_eq!(
            reaction_claims_answer(&room("!room:example.com"), &a, true, CHAT_EMOJI),
            Ok(4)
        );
    }

    /// `ExitPlanMode` never offers this option (`chat_option_index` is `None`) — the
    /// reaction is treated as unrecognized, the same as a stray emoji, not a crash or a
    /// panic on `unwrap`.
    #[test]
    fn chat_emoji_is_rejected_when_the_prompt_has_no_such_option() {
        let mut a = answer_in("!room:example.com", 3);
        a.chat_option_index = None;
        assert_eq!(
            reaction_claims_answer(&room("!room:example.com"), &a, true, CHAT_EMOJI),
            Err(ReactionRejection::NotANumberedReaction)
        );
    }
}
