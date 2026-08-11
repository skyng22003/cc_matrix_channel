//! Drives the tmux pane running the Matrix-facing Claude Code session — the only module
//! in this codebase that shells out (`grep -rn "std::process::Command\|tokio::process" src/`
//! is otherwise empty). Justified because `tmux capture-pane`/`send-keys` is already the
//! trusted mechanism `tools/restart-matrix.sh` runs in production; this generalizes it
//! rather than introducing a second way to drive the terminal.
//!
//! # Keystroke protocol — confirmed live, not guessed
//!
//! `AskUserQuestion` and `ExitPlanMode` do **not** share a keystroke protocol — confirmed
//! by direct observation in a disposable session, see `tools/menu-spike/FINDINGS.md`:
//!
//! - `AskUserQuestion`: a single digit keystroke, with no `Enter`, submits that option
//!   immediately, from wherever the cursor happens to be.
//! - `ExitPlanMode`: a bare digit is typed as literal text into the free-text "tell Claude
//!   what to change" option instead of selecting anything. Needs `Up`/`Down` navigation to
//!   the target option, then `Enter`.
//!
//! [`TmuxRelay::answer_prompt`] never guesses beyond what was directly observed — see its
//! doc comment for the ExitPlanMode cursor-position assumption it does still make, and why.
//!
//! # This never sends blind
//!
//! `restart-matrix.sh`'s own `send-keys` call has silently failed twice in production
//! (HANDOFF.md open items 3/9) with no confirmation step to catch it. Every send here goes
//! through [`TmuxRelay::send_and_confirm`], which recaptures the pane afterward and only
//! reports success if it actually changed as expected — and [`TmuxRelay::answer_prompt`]
//! additionally checks *before* sending that the pane still shows the expected prompt shape,
//! so a reaction arriving after someone already answered by hand doesn't inject a stray
//! keystroke into whatever screen came next.

use std::time::Duration;

use tokio::process::Command;

use crate::matrix::MenuAnswer;
use crate::pending_prompt::PromptKind;

/// How long to wait after sending keys before recapturing the pane to confirm they landed.
/// The CLI redraws its TUI well under this on a live, non-overloaded terminal; generous
/// rather than tight, since a false "didn't land" is only a retry, but a false "landed" from
/// checking too early could mask a real failure.
const SETTLE: Duration = Duration::from_millis(400);

/// How long [`TmuxRelay::answer_prompt`] keeps re-checking the pane before giving up on
/// its precondition check — not a settle for the keystroke itself (that's [`SETTLE`]), but
/// for the terminal's *own* render to catch up to the tool call. Confirmed live that this
/// race is real: `PreToolUse` hooks run synchronously and block the tool call until they
/// return, so the sidecar a caller detects can exist slightly *before* the CLI has
/// actually redrawn the prompt on screen — a reader fast enough to call `answer_prompt`
/// within that window would otherwise see a false "wrong pane."
const PRECONDITION_TIMEOUT: Duration = Duration::from_secs(3);
const PRECONDITION_POLL: Duration = Duration::from_millis(300);

pub struct TmuxRelay {
    pane: String,
}

impl TmuxRelay {
    pub fn new(pane: String) -> Self {
        Self { pane }
    }

    /// `tmux capture-pane -p -t <pane>` — the pane's currently visible content.
    pub async fn capture(&self) -> anyhow::Result<String> {
        let output = Command::new("tmux")
            .args(["capture-pane", "-p", "-t", &self.pane])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run tmux capture-pane: {e}"))?;
        if !output.status.success() {
            anyhow::bail!(
                "tmux capture-pane -t {} failed (status {:?}): {}",
                self.pane,
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// `tmux send-keys -t <pane> <keys...>` — each element is one send-keys argument: a
    /// literal string, or a key name tmux understands (`"Up"`, `"Down"`, `"Enter"`, ...).
    /// Never appends `Enter` itself; callers decide, since the two prompt kinds confirmed
    /// live to disagree on exactly that.
    async fn send_keys(&self, keys: &[&str]) -> anyhow::Result<()> {
        let mut cmd = Command::new("tmux");
        cmd.args(["send-keys", "-t", &self.pane]);
        cmd.args(keys);
        let status = cmd
            .status()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run tmux send-keys: {e}"))?;
        if !status.success() {
            anyhow::bail!(
                "tmux send-keys -t {} failed (status {:?})",
                self.pane,
                status.code()
            );
        }
        Ok(())
    }

    /// Send `keys`, wait [`SETTLE`], recapture, and only report success if
    /// `confirmed(before, after)` says the pane actually changed as expected.
    ///
    /// Never retries blind: a caller wanting another attempt must re-derive fresh keys from
    /// the new capture, since a stale second `send_keys` risks double-selecting (e.g. a
    /// second digit landing on whatever screen came after the first one already succeeded).
    pub async fn send_and_confirm(
        &self,
        keys: &[&str],
        settle: Duration,
        confirmed: impl Fn(&str, &str) -> bool,
    ) -> anyhow::Result<bool> {
        let before = self.capture().await?;
        self.send_keys(keys).await?;
        tokio::time::sleep(settle).await;
        let after = self.capture().await?;
        Ok(confirmed(&before, &after))
    }

    /// Answer a pending menu prompt for real.
    ///
    /// Checks the pane shows the expected prompt shape (right option count — from
    /// [`MenuAnswer::option_count`], the same count [`crate::matrix::PendingAnswer`] was
    /// validated against when the reaction was claimed, not just a lower bound inferred
    /// from which option was picked — and the kind's own footer text) *before* sending
    /// anything, retrying for [`PRECONDITION_TIMEOUT`] to absorb the hook/render race
    /// above. Refuses rather than guesses if the shape still doesn't match once that's
    /// exhausted, since that means either the wrong pane, or someone already answered by
    /// hand — a real "not this prompt," not just "not yet."
    ///
    /// **`ExitPlanMode`'s cursor-position assumption, stated plainly rather than hidden**:
    /// every fresh `ExitPlanMode` prompt observed during the spike started with the cursor
    /// on option 1 (`tools/menu-spike/fixture-exitplanmode-initial.txt`), so this walks
    /// `Down` from index 0 rather than reading the cursor's actual position out of the
    /// capture first. Not yet proven against a prompt a human has already nudged by hand —
    /// tracked as a known gap, not silently assumed safe. The pre-send shape check reduces
    /// but does not eliminate the risk: it confirms *a* matching prompt is showing, not that
    /// the cursor sits where this method assumes it does.
    pub async fn answer_prompt(&self, answer: &MenuAnswer) -> anyhow::Result<bool> {
        let deadline = tokio::time::Instant::now() + PRECONDITION_TIMEOUT;
        loop {
            let pane = self.capture().await?;
            if pane_shows_expected_prompt(&pane, answer.kind, answer.option_count) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "pane does not show the expected {:?} prompt with {} option(s) after \
                     {PRECONDITION_TIMEOUT:?} of retrying; refusing to send blind",
                    answer.kind,
                    answer.option_count
                );
            }
            tokio::time::sleep(PRECONDITION_POLL).await;
        }

        match answer.kind {
            PromptKind::AskUserQuestion => {
                // Confirmed live: a single digit, no Enter, submits immediately from
                // anywhere. The CLI numbers from 1; `option_index` is zero-based.
                let digit = (answer.option_index + 1).to_string();
                self.send_and_confirm(&[digit.as_str()], SETTLE, |before, after| before != after)
                    .await
            }
            PromptKind::ExitPlanMode => {
                // Confirmed live: no digit shortcut, and a bare digit corrupts the
                // free-text option instead of selecting anything (see the module doc and
                // this method's own doc for the cursor-position caveat).
                let mut keys: Vec<&str> =
                    std::iter::repeat_n("Down", answer.option_index).collect();
                keys.push("Enter");
                self.send_and_confirm(&keys, SETTLE, |before, after| before != after)
                    .await
            }
        }
    }
}

/// Structural precondition check: does `pane` show a numbered menu with at least
/// `expected_option_count` options and this prompt kind's own footer text?
///
/// Deliberately shape-based, not content-based — this module never reads (or is given)
/// the actual question/plan text, only the count and kind `crate::live_status` already
/// tracked when it posted the prompt to Matrix. A false positive here (some unrelated
/// screen that happens to match the shape) is possible in principle but was not observed
/// against any of the three real prompts captured during the spike
/// (`tools/menu-spike/fixture-*.txt`).
///
/// Matches against whitespace-flattened text, not the raw capture — confirmed live that
/// `tmux capture-pane`'s hard line-wrapping can split a target phrase across two lines
/// (`"...ready to execute. Would you\n   like to proceed?"` at 100 columns), which would
/// otherwise make a plain substring check fail forever regardless of how long a caller
/// retries. This is exactly what happened first: `answer_prompt`'s retry loop ran out its
/// full timeout against a pane that visibly *did* show the right prompt the whole time.
fn pane_shows_expected_prompt(pane: &str, kind: PromptKind, expected_option_count: usize) -> bool {
    let flat = pane.split_whitespace().collect::<Vec<_>>().join(" ");
    let numbered_options_present = (1..=expected_option_count)
        .filter(|n| flat.contains(&format!("{n}.")))
        .count();
    if numbered_options_present < expected_option_count {
        return false;
    }
    match kind {
        PromptKind::AskUserQuestion => flat.contains("Enter to select"),
        PromptKind::ExitPlanMode => flat.contains("Would you like to proceed?"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmux_available() -> bool {
        std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .is_ok()
    }

    fn unique_session_name() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("cc_matrix_channel_test_{}_{nanos}", std::process::id())
    }

    /// Kills the disposable test session on drop — including during a panicking test, since
    /// `Drop::drop` still runs while unwinding. Uses blocking `std::process::Command` rather
    /// than `tokio::process`: `drop` can't be `async`, and a quick synchronous cleanup call
    /// is fine for a test fixture.
    struct SessionGuard(String);

    impl Drop for SessionGuard {
        fn drop(&mut self) {
            let _ = std::process::Command::new("tmux")
                .args(["kill-session", "-t", &self.0])
                .status();
        }
    }

    /// Spawns a disposable tmux session running `cat` (never `claude-matrix`), hands the
    /// caller a `TmuxRelay` pointed at it, and tears it down afterward — including if the
    /// closure panics, via [`SessionGuard`].
    async fn with_test_session<F, Fut>(f: F)
    where
        F: FnOnce(TmuxRelay) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        if !tmux_available() {
            eprintln!("skipping: tmux binary not available in this environment");
            return;
        }
        let session = unique_session_name();
        let start = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &session,
                "-x",
                "80",
                "-y",
                "24",
                "cat",
            ])
            .status()
            .await
            .expect("spawn tmux new-session");
        assert!(start.success(), "failed to create disposable test session");
        let _guard = SessionGuard(session.clone());

        let relay = TmuxRelay::new(session);
        f(relay).await;
    }

    #[tokio::test]
    async fn capture_reads_what_was_sent() {
        with_test_session(|relay| async move {
            relay.send_keys(&["hello-from-relay"]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            let pane = relay.capture().await.unwrap();
            assert!(pane.contains("hello-from-relay"), "pane: {pane}");
        })
        .await;
    }

    #[tokio::test]
    async fn send_and_confirm_reports_true_on_a_real_change() {
        with_test_session(|relay| async move {
            let ok = relay
                .send_and_confirm(
                    &["distinct-marker-xyz"],
                    Duration::from_millis(200),
                    |before, after| {
                        !before.contains("distinct-marker-xyz")
                            && after.contains("distinct-marker-xyz")
                    },
                )
                .await
                .unwrap();
            assert!(ok);
        })
        .await;
    }

    #[tokio::test]
    async fn send_and_confirm_reports_false_when_nothing_matches() {
        with_test_session(|relay| async move {
            let ok = relay
                .send_and_confirm(
                    &["irrelevant"],
                    Duration::from_millis(200),
                    |_before, after| after.contains("this-text-will-never-appear"),
                )
                .await
                .unwrap();
            assert!(!ok);
        })
        .await;
    }

    #[test]
    fn pane_shape_check_requires_all_numbered_options_present() {
        let pane = "1. Yes\n2. No\nEnter to select";
        assert!(pane_shows_expected_prompt(
            pane,
            PromptKind::AskUserQuestion,
            2
        ));
        assert!(!pane_shows_expected_prompt(
            pane,
            PromptKind::AskUserQuestion,
            3
        ));
    }

    #[test]
    fn pane_shape_check_distinguishes_prompt_kind_by_footer() {
        let ask_pane = "1. Yes\n2. No\nEnter to select \u{b7} Esc to cancel";
        let plan_pane = "1. Yes\n2. No\nWould you like to proceed?";
        assert!(pane_shows_expected_prompt(
            ask_pane,
            PromptKind::AskUserQuestion,
            2
        ));
        assert!(!pane_shows_expected_prompt(
            ask_pane,
            PromptKind::ExitPlanMode,
            2
        ));
        assert!(pane_shows_expected_prompt(
            plan_pane,
            PromptKind::ExitPlanMode,
            2
        ));
        assert!(!pane_shows_expected_prompt(
            plan_pane,
            PromptKind::AskUserQuestion,
            2
        ));
    }

    /// Regression pin for a real bug caught live: `tmux capture-pane`'s hard line-wrapping
    /// split "Would you like to proceed?" across two lines at 100 columns
    /// (`"...execute. Would you\n   like to proceed?"`), which made a plain substring
    /// check fail *forever* — `answer_prompt`'s retry loop ran out its full timeout
    /// against a pane that visibly did show the right prompt the whole time. Also covers
    /// a wrapped numbered option, the other half of the same check.
    #[test]
    fn pane_shape_check_tolerates_hard_line_wrapping() {
        let wrapped_footer =
            "1. Yes\n2. No\n3. Maybe\nClaude has written up a plan. Would you\nlike to proceed?";
        assert!(pane_shows_expected_prompt(
            wrapped_footer,
            PromptKind::ExitPlanMode,
            3
        ));

        let wrapped_option = "1. Yes, and use auto\nmode\n2. No\nWould you like to proceed?";
        assert!(pane_shows_expected_prompt(
            wrapped_option,
            PromptKind::ExitPlanMode,
            2
        ));
    }

    // --- Live-CLI tests: the real hook -> sidecar -> answer_prompt pipeline ---
    //
    // The tests above prove the tmux mechanism (capture/send/confirm) against a plain
    // `cat` pane. These prove the *whole* live pipeline against the real `claude` CLI:
    // `scripts/pending-prompt-hook.sh` actually firing and writing the sidecar,
    // `pending_prompt::read_pending_prompt_for_session` actually reading it, and
    // `answer_prompt` reproducing the keystroke protocol Task 0's spike confirmed by hand
    // — against a disposable session only (never `claude-matrix`), same MCP/trust-prompt
    // dismissal discipline as the spike.
    //
    // Superseded a transcript-polling version of these same two tests (2026-08-11): that
    // version failed deterministically, not flakily, once it was understood *why* — see
    // `tools/menu-spike/FINDINGS.md`'s two correction sections for the full story of how
    // this hook-based design was arrived at.
    //
    // Ignored by default: spawns a real Claude Code process (real API usage), needs
    // `claude`, `tmux`, and `jq` (the hook script's own dependency) on PATH.
    //
    //   cargo test --bin cc_matrix_channel tmux_relay::tests::live_cli -- --ignored --nocapture

    mod live_cli {
        use super::*;
        use crate::matrix::MenuAnswer;
        use crate::pending_prompt::read_pending_prompt_for_session_at;
        use std::path::{Path, PathBuf};

        fn claude_available() -> bool {
            std::process::Command::new("claude")
                .arg("--version")
                .output()
                .is_ok()
        }

        fn jq_available() -> bool {
            std::process::Command::new("jq")
                .arg("--version")
                .output()
                .is_ok()
        }

        /// Mirrors `status.rs`'s private `slug_for_cwd` — duplicated rather than exposed
        /// crate-wide just for this test, since the two must never actually diverge (both
        /// derive from the same Claude Code project-directory convention) but there's no
        /// production reason for `tmux_relay` to depend on that detail of `status.rs`.
        fn project_dir_for_cwd(cwd: &Path) -> PathBuf {
            let home = dirs_next::home_dir().expect("HOME must be set");
            let slug: String = cwd
                .to_string_lossy()
                .chars()
                .map(|c| {
                    if c == '/' || c == '.' || c == '_' {
                        '-'
                    } else {
                        c
                    }
                })
                .collect();
            home.join(".claude").join("projects").join(slug)
        }

        async fn newest_transcript(project_dir: &Path) -> Option<PathBuf> {
            let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
            let mut rd = tokio::fs::read_dir(project_dir).await.ok()?;
            while let Ok(Some(entry)) = rd.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Ok(meta) = entry.metadata().await else {
                    continue;
                };
                let Ok(mtime) = meta.modified() else {
                    continue;
                };
                if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
                    best = Some((mtime, path));
                }
            }
            best.map(|(_, p)| p)
        }

        /// The transcript filename *is* the session id (confirmed throughout this
        /// project's own transcript-reading code) — cheaper and just as reliable as
        /// parsing it back out of a JSON record.
        async fn wait_for_session_id(project_dir: &Path, timeout: Duration) -> Option<String> {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                if let Some(path) = newest_transcript(project_dir).await
                    && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                {
                    return Some(stem.to_string());
                }
                if tokio::time::Instant::now() >= deadline {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }

        async fn wait_for_pending_prompt(
            sidecar: &Path,
            session_id: &str,
            timeout: Duration,
        ) -> Option<crate::pending_prompt::PendingPrompt> {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                if let Some(p) = read_pending_prompt_for_session_at(
                    sidecar,
                    Some(session_id),
                    std::time::SystemTime::now(),
                ) {
                    return Some(p);
                }
                if tokio::time::Instant::now() >= deadline {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }

        /// The other direction: true once the `PostToolUse` half of the hook has cleared
        /// the sidecar (or it was already clear), false if it's still there after `timeout`.
        async fn wait_for_resolved(sidecar: &Path, session_id: &str, timeout: Duration) -> bool {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                if read_pending_prompt_for_session_at(
                    sidecar,
                    Some(session_id),
                    std::time::SystemTime::now(),
                )
                .is_none()
                {
                    return true;
                }
                if tokio::time::Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }

        /// Absolute path to `scripts/pending-prompt-hook.sh` — `CARGO_MANIFEST_DIR` is a
        /// compile-time env var pointing at the crate root regardless of `cargo test`'s
        /// invocation cwd, so this doesn't depend on where the test happens to be run from.
        fn hook_script_path() -> PathBuf {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("scripts")
                .join("pending-prompt-hook.sh")
        }

        /// Spawns a disposable `claude` session (never `claude-matrix`), with a
        /// `--settings` file scoping `PreToolUse`/`PostToolUse` hooks to
        /// `scripts/pending-prompt-hook.sh` and a sidecar path unique to this test run —
        /// isolated from any real bridge's sidecar and from every other test. Rejects the
        /// MCP-server-enable prompt so it can never reach the live Matrix bot account.
        async fn spawn_disposable_claude_session()
        -> Option<(TmuxRelay, PathBuf, PathBuf, SessionGuard)> {
            if !tmux_available() || !claude_available() || !jq_available() {
                eprintln!("skipping: tmux, claude, and/or jq not available");
                return None;
            }
            let unique = unique_session_name();
            let scratch =
                std::env::temp_dir().join(format!("cc_matrix_channel_live_probe_{unique}"));
            tokio::fs::create_dir_all(&scratch).await.ok()?;
            let project_dir = project_dir_for_cwd(&scratch);
            let sidecar = std::env::temp_dir()
                .join(format!("cc_matrix_channel_pending_prompt_{unique}.json"));
            let _ = tokio::fs::remove_file(&sidecar).await;

            let settings = format!(
                r#"{{"hooks":{{"PreToolUse":[{{"matcher":"AskUserQuestion|ExitPlanMode","hooks":[{{"type":"command","command":"{script}"}}]}}],"PostToolUse":[{{"matcher":"AskUserQuestion|ExitPlanMode","hooks":[{{"type":"command","command":"{script}"}}]}}]}}}}"#,
                script = hook_script_path().display()
            );
            let settings_path =
                std::env::temp_dir().join(format!("cc_matrix_channel_hook_settings_{unique}.json"));
            tokio::fs::write(&settings_path, &settings).await.ok()?;

            // The session's own command is a plain shell, not `claude` directly: if
            // `claude` were the pane's command and it exited or errored, the whole session
            // would vanish with it (confirmed the hard way — this is exactly what happened
            // on the first attempt at the transcript-based version of this test). Running
            // it as a command sent into a live shell, the way Task 0's manual spike did,
            // means the pane survives regardless.
            let session = unique_session_name();
            let start = Command::new("tmux")
                .args([
                    "new-session",
                    "-d",
                    "-s",
                    &session,
                    "-c",
                    scratch.to_str().unwrap(),
                    "-x",
                    "100",
                    "-y",
                    "30",
                ])
                .status()
                .await
                .ok()?;
            assert!(start.success(), "failed to spawn disposable session");
            let guard = SessionGuard(session.clone());
            let relay = TmuxRelay::new(session);

            // Exported before `claude` starts, so the hook script (which inherits the
            // pane's shell environment) picks it up on every invocation.
            relay
                .send_keys(&[&format!(
                    "export CC_MATRIX_PENDING_PROMPT_PATH={}",
                    sidecar.display()
                )])
                .await
                .ok()?;
            relay.send_keys(&["Enter"]).await.ok()?;
            tokio::time::sleep(Duration::from_millis(300)).await;

            relay
                .send_keys(&[&format!("claude --settings {}", settings_path.display())])
                .await
                .ok()?;
            relay.send_keys(&["Enter"]).await.ok()?;
            dismiss_startup_prompts(&relay, Duration::from_secs(15)).await;

            Some((relay, project_dir, sidecar, guard))
        }

        /// Handles whichever of Claude Code's startup prompts actually appears, by
        /// structurally matching pane content rather than assuming a fixed sequence or
        /// timing — confirmed live that a brand-new scratch directory shows a
        /// trust-this-folder prompt Task 0's spike never saw (its directory was already
        /// trusted), while a directory that inherits a parent `.mcp.json` additionally
        /// shows the MCP-server-enable prompt. Stops once the plain ready prompt shows.
        async fn dismiss_startup_prompts(relay: &TmuxRelay, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            // One-shot guards: confirmed live that sending the same dismissal key twice
            // (a second capture landing before the UI has redrawn past the first send)
            // makes the second one land as a stray keystroke in the now-ready chat input
            // box instead — this is what actually broke the first automated attempt at
            // this test, not the prompt-matching itself.
            let mut trust_accepted = false;
            let mut mcp_rejected = false;
            loop {
                if tokio::time::Instant::now() >= deadline {
                    return;
                }
                let Ok(pane) = relay.capture().await else {
                    return;
                };
                // Check readiness first, before any dismissal logic below — once this
                // matches there is nothing left to dismiss.
                if pane.contains("manual mode on") {
                    return;
                }
                if !trust_accepted
                    && pane.contains("Is this a project you created or one you trust?")
                {
                    let _ = relay.send_keys(&["1"]).await;
                    trust_accepted = true;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                if !mcp_rejected && pane.contains("Select any you wish to enable") {
                    // Reject all — never enable a real MCP server (in particular `matrix`)
                    // from a disposable test session.
                    let _ = relay.send_keys(&["Escape"]).await;
                    mcp_rejected = true;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
        }

        #[tokio::test]
        #[ignore = "spawns a real disposable Claude Code session; needs `claude` + `tmux` + `jq`, real API usage"]
        async fn answer_prompt_selects_the_right_option_in_a_real_ask_user_question() {
            let Some((relay, project_dir, sidecar, _guard)) =
                spawn_disposable_claude_session().await
            else {
                return;
            };

            relay
                .send_keys(&[
                    "Use the AskUserQuestion tool right now to ask me to pick a fruit from: \
                     Apple, Banana, Cherry. Do not do anything else first.",
                ])
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
            relay.send_keys(&["Enter"]).await.unwrap();

            let session_id = wait_for_session_id(&project_dir, Duration::from_secs(15))
                .await
                .expect("a transcript should appear for the disposable session");
            let pending = wait_for_pending_prompt(&sidecar, &session_id, Duration::from_secs(30))
                .await
                .expect(
                    "the PreToolUse hook should have written the sidecar while the question \
                     is still pending",
                );
            assert_eq!(pending.kind, PromptKind::AskUserQuestion);
            assert_eq!(pending.options.len(), 3);

            // Pick index 1 (Banana) — not the default cursor position, so a passing result
            // actually proves the digit-select path, not just "something got dismissed."
            let answer = MenuAnswer {
                tool_use_id: pending.tool_use_id.clone(),
                kind: pending.kind,
                option_index: 1,
                option_count: pending.options.len(),
            };
            let confirmed = relay
                .answer_prompt(&answer)
                .await
                .expect("answer_prompt should not error");
            assert!(
                confirmed,
                "answer_prompt should confirm the keystroke landed"
            );

            // Confirm the PostToolUse half of the hook cleared the sidecar too, not just
            // that the pane changed shape.
            assert!(
                wait_for_resolved(&sidecar, &session_id, Duration::from_secs(5)).await,
                "the PostToolUse hook should have cleared the sidecar once answered"
            );
        }

        #[tokio::test]
        #[ignore = "spawns a real disposable Claude Code session; needs `claude` + `tmux` + `jq`, real API usage"]
        async fn answer_prompt_confirms_a_real_exit_plan_mode() {
            let Some((relay, project_dir, sidecar, _guard)) =
                spawn_disposable_claude_session().await
            else {
                return;
            };

            relay
                .send_keys(&[
                    "Switch into plan mode and make a trivial 2-step plan for renaming a \
                     variable in a nonexistent file called foo.txt, then present the plan \
                     for approval.",
                ])
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
            relay.send_keys(&["Enter"]).await.unwrap();

            let session_id = wait_for_session_id(&project_dir, Duration::from_secs(15))
                .await
                .expect("a transcript should appear for the disposable session");
            let pending = wait_for_pending_prompt(&sidecar, &session_id, Duration::from_secs(60))
                .await
                .expect(
                    "the PreToolUse hook should have written the sidecar while the plan \
                     approval is still pending",
                );
            assert_eq!(pending.kind, PromptKind::ExitPlanMode);
            assert_eq!(pending.options.len(), 3);

            // Index 1 = "Yes, manually approve edits" — exercises the Down-navigation
            // path (index 0 would pass even with a broken navigation loop, since the
            // cursor already starts there).
            let answer = MenuAnswer {
                tool_use_id: pending.tool_use_id.clone(),
                kind: pending.kind,
                option_index: 1,
                option_count: pending.options.len(),
            };
            let confirmed = relay
                .answer_prompt(&answer)
                .await
                .expect("answer_prompt should not error");
            assert!(
                confirmed,
                "answer_prompt should confirm the keystroke landed"
            );

            assert!(
                wait_for_resolved(&sidecar, &session_id, Duration::from_secs(5)).await,
                "the PostToolUse hook should have cleared the sidecar once answered"
            );
        }
    }
}
