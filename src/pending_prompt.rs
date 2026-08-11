//! Detection of Claude Code's own blocking CLI prompts — `AskUserQuestion` and
//! `ExitPlanMode` — via a `PreToolUse`/`PostToolUse` hook sidecar file, not the transcript.
//!
//! # Why not the transcript (tried first, confirmed broken)
//!
//! An earlier version of this module read the transcript tail, on the theory that Claude
//! Code writes the `tool_use` record the moment the CLI renders the menu, before any
//! `tool_result` resolves it. **Confirmed live that this is wrong**: the record does not
//! land in the transcript until the prompt is *resolved* — verified across three disposable
//! sessions, both tool kinds, with the prompt still visibly open on screen for up to 60s of
//! real waiting. See `tools/menu-spike/FINDINGS.md`'s first correction section for the full
//! evidence, and its second section for how the current design was found instead: a
//! `PreToolUse` hook *does* fire before the block, with the full `tool_input` already
//! attached, confirmed the same way.
//!
//! # This design
//!
//! `scripts/pending-prompt-hook.sh`, wired into `PreToolUse`/`PostToolUse` for these two
//! tool names (not done automatically by anything in this repo — see the script's own doc
//! comment and the README for the settings.json snippet and why deploying it to the live
//! bridge session is a separate, deliberate step), writes the hook's own stdin JSON to a
//! sidecar file on `PreToolUse` and removes it on a matching `PostToolUse`. This module
//! just reads that file.
//!
//! Because the hook is configured at the project level, it fires for *every* Claude Code
//! session sharing that project directory, not just the bridge's own — the hook script has
//! no way to know which session is "the bridge." So the sidecar's `session_id` is checked
//! against `CLAUDE_CODE_SESSION_ID` here, in the reader, before it's ever trusted. A sidecar
//! written by a foreign session reads as `None`, the same as no sidecar at all.
//!
//! # Privacy
//!
//! `crate::status` is deliberately free of tool inputs — see its module doc. This module is
//! the second deliberate exception, alongside [`crate::fallback_reply`]: it reads the
//! `input` of exactly two tool names, and only because that input *is* the question or plan
//! that has to be shown to answer it — there is no metadata-only way to render "what does
//! the menu say." No other tool's input is ever read here; a sidecar naming any other tool
//! is simply not a pending prompt as far as this module is concerned.
//!
//! # Keystroke protocol lives elsewhere
//!
//! This module only extracts *what* is being asked. How an answer gets typed back into the
//! terminal (confirmed by a live spike to differ between the two tools — see
//! `tools/menu-spike/FINDINGS.md`) is `crate::tmux_relay`'s concern, not this one's.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// The two tool names this module recognizes. No other tool's sidecar entry counts as a
/// pending prompt.
pub const ASK_USER_QUESTION_TOOL: &str = "AskUserQuestion";
pub const EXIT_PLAN_MODE_TOOL: &str = "ExitPlanMode";

/// Reactions only go up to nine unambiguous keycap emoji (1️⃣–9️⃣). A prompt with more
/// options than this cannot be answered by reaction — see [`PendingPrompt::answerable_by_reaction`].
pub const MAX_REACTION_OPTIONS: usize = 9;

/// `ExitPlanMode`'s three options are fixed CLI chrome, not part of the tool's `input` —
/// confirmed live (`tools/menu-spike/fixture-exitplanmode-initial.txt`). Option 3 is a
/// free-text box ("tell Claude what to change"); answering it by reaction alone selects it
/// but cannot supply the follow-up text — left to the caller (posting/relay code) to decide
/// whether to offer it as a reaction option at all.
pub const EXIT_PLAN_MODE_OPTIONS: [&str; 3] = [
    "Yes, and use auto mode",
    "Yes, manually approve edits",
    "Tell Claude what to change",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    AskUserQuestion,
    ExitPlanMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPrompt {
    /// The tool_use block's own `id` — identity for matching a later resolution (or a
    /// later reaction) back to *this* prompt specifically, not just "a" prompt. Confirmed
    /// live that both tools' `tool_result` blocks carry `tool_use_id` matching this
    /// exactly, so resolution is matched by id, not by position.
    pub tool_use_id: String,
    pub kind: PromptKind,
    pub header: Option<String>,
    pub question: Option<String>,
    /// Rendered option labels, in the CLI's own display order. Reaction index N answers
    /// `options[N]`.
    pub options: Vec<String>,
    /// `ExitPlanMode`'s plan markdown. `None` for `AskUserQuestion`.
    pub plan: Option<String>,
    /// True when the raw `input` had a shape this module doesn't render as a single
    /// numbered menu — `AskUserQuestion` with `multiSelect: true`, or more than one
    /// question in the `questions` array. Forces [`answerable_by_reaction`] to `false`
    /// regardless of option count: a live spike only confirmed the keystroke protocol for
    /// the single-question, single-select case, and this module does not guess beyond what
    /// was observed.
    ///
    /// [`answerable_by_reaction`]: PendingPrompt::answerable_by_reaction
    pub unsupported_shape: bool,
}

impl PendingPrompt {
    pub fn answerable_by_reaction(&self) -> bool {
        !self.unsupported_shape
            && !self.options.is_empty()
            && self.reaction_option_count() <= MAX_REACTION_OPTIONS
    }

    /// How many of `options`, from the front, are safe to answer with a single reaction.
    ///
    /// For `ExitPlanMode` specifically, the last of the three fixed options ("Tell Claude
    /// what to change") is a free-text box — a code review caught that offering it as a
    /// reaction target was documented as an open question (see [`EXIT_PLAN_MODE_OPTIONS`]'s
    /// own doc) but never actually resolved: nothing had confirmed live what pressing
    /// `Enter` on that option with no typed feedback actually does, and
    /// `TmuxRelay::send_and_confirm`'s "did the pane change at all" check would report
    /// success even if it left the CLI stuck waiting in that box. Excluded from the
    /// reaction-eligible count until that's verified — still rendered in the message body
    /// for visibility, just not offered as a numbered reaction target. Every other case
    /// (`AskUserQuestion`, or any `ExitPlanMode` option before the last) is unaffected.
    pub fn reaction_option_count(&self) -> usize {
        match self.kind {
            PromptKind::ExitPlanMode => self.options.len().saturating_sub(1),
            PromptKind::AskUserQuestion => self.options.len(),
        }
    }
}

/// A sidecar older than this is treated as stale and ignored — same posture as
/// `status.rs`'s stall detection, applied here for a different reason: a hook fires
/// `PreToolUse` and the matching `PostToolUse` normally lands within seconds, so a much
/// older entry means the writing process was killed mid-prompt (Ctrl-C, OOM, host crash)
/// before it could clear its own sidecar. `tmux_relay`'s pane-shape precondition check
/// already refuses to send a keystroke into a pane that doesn't match, so a stale entry
/// can't cause a wrong keystroke — but without this it could still cause a spurious
/// "Claude is asking…" post to Matrix if the same session id is ever reused (e.g. a
/// `--resume`d session), long after the original prompt is moot.
const MAX_SIDECAR_AGE: Duration = Duration::from_secs(300);

/// Where `scripts/pending-prompt-hook.sh` writes the sidecar for a given session, and
/// where this module reads it back from — namespaced per `session_id`, not one shared
/// path.
///
/// **Why per-session, not shared**: `scripts/pending-prompt-hook.sh` is wired in via
/// project-local or user-level `settings.json` (see README's "Menu forwarding" section),
/// which means it fires for *every* Claude Code session sharing that scope, not just the
/// bridge's own. An earlier version of this function returned one shared path, with only
/// [`read_pending_prompt_for_session`]'s `session_id` check guarding against a foreign
/// session's entry being *misread* as the bridge's own. A code review caught that this
/// does nothing to stop a foreign session's write from *clobbering* the bridge's own
/// still-pending entry — the bridge's next poll would then see no sidecar for its own
/// session id, read that as "resolved," and edit the Matrix message to say so while the
/// terminal was in fact still blocked. Giving every session its own file removes the
/// collision instead of just detecting it after the fact.
///
/// `CC_MATRIX_PENDING_PROMPT_PATH`, if set, overrides the path outright regardless of
/// `session_id` — a full-path override, not a directory, used by tests that want an
/// isolated file without needing a real session id. Same env var name the hook script
/// itself honours, so the two stay in sync without hardcoding the override twice.
pub fn pending_prompt_path(session_id: &str) -> PathBuf {
    if let Ok(p) = std::env::var("CC_MATRIX_PENDING_PROMPT_PATH") {
        return PathBuf::from(p);
    }
    dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("channels")
        .join("matrix")
        .join(format!("pending_prompt-{session_id}.json"))
}

/// Core logic, session-matching and "now" pulled out as explicit parameters — same
/// pattern `status::resolve_transcript`/`read_status_at` use for
/// `CLAUDE_CODE_SESSION_ID`/the current time, so tests never have to mutate a
/// process-global env var (which races under Rust's default parallel test runner) or sleep
/// in real time to exercise staleness.
///
/// `None` covers "no sidecar file," "malformed sidecar," "sidecar names a different
/// session," "sidecar names some other tool," and "sidecar is older than
/// [`MAX_SIDECAR_AGE`]" alike — the same shape every other `None` in this codebase's
/// status-reading already follows.
pub(crate) fn read_pending_prompt_for_session_at(
    path: &Path,
    expected_session_id: Option<&str>,
    now: SystemTime,
) -> Option<PendingPrompt> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;

    let expected = expected_session_id?;
    let session_id = value.get("session_id").and_then(|s| s.as_str())?;
    if session_id != expected {
        return None;
    }

    let written_at = value
        .get("_bridge_written_at_unix")
        .and_then(|t| t.as_u64())?;
    let age = now
        .duration_since(UNIX_EPOCH + Duration::from_secs(written_at))
        .ok()?;
    if age > MAX_SIDECAR_AGE {
        return None;
    }

    let tool_use_id = value
        .get("tool_use_id")
        .and_then(|s| s.as_str())?
        .to_string();
    let name = value.get("tool_name").and_then(|s| s.as_str())?;
    let input = value.get("tool_input").cloned().unwrap_or(Value::Null);
    build_prompt(tool_use_id, name, &input)
}

/// Convenience wrapper: resolves the per-session sidecar path and the expected session id
/// from the environment, the way `crate::status::read_status` wraps `read_status_at`.
pub fn read_pending_prompt() -> Option<PendingPrompt> {
    let session_id = std::env::var("CLAUDE_CODE_SESSION_ID").ok()?;
    let path = pending_prompt_path(&session_id);
    read_pending_prompt_for_session_at(&path, Some(&session_id), SystemTime::now())
}

fn build_prompt(tool_use_id: String, name: &str, input: &Value) -> Option<PendingPrompt> {
    match name {
        ASK_USER_QUESTION_TOOL => parse_ask_user_question(tool_use_id, input),
        EXIT_PLAN_MODE_TOOL => Some(parse_exit_plan_mode(tool_use_id, input)),
        _ => None,
    }
}

/// Confirmed live shape: `{"questions":[{"question","header","options":[{"label","description"}],"multiSelect"}]}`
/// (`tools/menu-spike/pretooluse-probe.log`). Only the first question is rendered;
/// `unsupported_shape` is set — never silently dropped — when there's more than one, or
/// `multiSelect` is true.
fn parse_ask_user_question(tool_use_id: String, input: &Value) -> Option<PendingPrompt> {
    let questions = input.get("questions")?.as_array()?;
    let first = questions.first()?;

    let header = first
        .get("header")
        .and_then(|h| h.as_str())
        .map(str::to_string);
    let question = first
        .get("question")
        .and_then(|q| q.as_str())
        .map(str::to_string);
    let options: Vec<String> = first
        .get("options")
        .and_then(|o| o.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|o| o.get("label").and_then(|l| l.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let multi_select = first
        .get("multiSelect")
        .and_then(|m| m.as_bool())
        .unwrap_or(false);

    Some(PendingPrompt {
        tool_use_id,
        kind: PromptKind::AskUserQuestion,
        header,
        question,
        options,
        plan: None,
        unsupported_shape: multi_select || questions.len() > 1,
    })
}

/// Confirmed live shape: `{"plan":"markdown","planFilePath":"..."}`
/// (`tools/menu-spike/pretooluse-probe.log`). The three options are not part of `input` at
/// all — they're fixed CLI chrome, `EXIT_PLAN_MODE_OPTIONS` — so this always succeeds even
/// if `plan` itself is missing or non-string.
fn parse_exit_plan_mode(tool_use_id: String, input: &Value) -> PendingPrompt {
    let plan = input
        .get("plan")
        .and_then(|p| p.as_str())
        .map(str::to_string);
    PendingPrompt {
        tool_use_id,
        kind: PromptKind::ExitPlanMode,
        header: None,
        question: None,
        options: EXIT_PLAN_MODE_OPTIONS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        plan,
        unsupported_shape: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const SESSION: &str = "test-session-abc";

    // Fixed fixture "written at" time and a "now" a few seconds later — every fixture
    // below embeds this exact timestamp, so ordinary tests exercise the fresh-sidecar path
    // without being sensitive to real wall-clock time. The staleness tests derive their own
    // `now` values from this same anchor instead of a separate magic number.
    const WRITTEN_AT_UNIX: u64 = 1_800_000_000;
    fn shortly_after() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(WRITTEN_AT_UNIX + 5)
    }

    fn write_sidecar(dir: &tempfile::TempDir, contents: &str) -> PathBuf {
        let path = dir.path().join("pending_prompt.json");
        fs::write(&path, contents).unwrap();
        path
    }

    fn pending_at(
        contents: &str,
        expected_session: Option<&str>,
        now: SystemTime,
    ) -> Option<PendingPrompt> {
        let dir = tempfile::tempdir().unwrap();
        let path = write_sidecar(&dir, contents);
        read_pending_prompt_for_session_at(&path, expected_session, now)
    }

    fn pending_of(contents: &str, expected_session: Option<&str>) -> Option<PendingPrompt> {
        pending_at(contents, expected_session, shortly_after())
    }

    // Real captured shapes from tools/menu-spike/pretooluse-probe.log — field names and
    // nesting match exactly what Claude Code v2.1.220's PreToolUse hook actually sent, not
    // a guessed schema (except `_bridge_written_at_unix`, which the hook script adds
    // itself rather than Claude Code — see scripts/pending-prompt-hook.sh). Trimmed to the
    // fields this module reads.
    const ASK_SIDECAR: &str = r#"{"session_id":"test-session-abc","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_use_id":"toolu_ask1","tool_input":{"questions":[{"question":"Pick a fruit:","header":"Fruit","options":[{"label":"Apple","description":"Apple"},{"label":"Banana","description":"Banana"},{"label":"Cherry","description":"Cherry"}],"multiSelect":false}]},"_bridge_written_at_unix":1800000000}"#;

    // Double-hash delimiter: the plan markdown's own leading "# " heading produces a
    // literal `"#` right after the JSON key, which would otherwise close a single-hash raw
    // string early (confirmed the hard way, back when this lived in the transcript-reading
    // version of this module's tests).
    const PLAN_SIDECAR: &str = r##"{"session_id":"test-session-abc","hook_event_name":"PreToolUse","tool_name":"ExitPlanMode","tool_use_id":"toolu_plan1","tool_input":{"plan":"# Rename a variable in foo.txt\n\nSteps here.\n","planFilePath":"/home/node/.claude/plans/demo.md"},"_bridge_written_at_unix":1800000000}"##;

    const BASH_SIDECAR: &str = r#"{"session_id":"test-session-abc","hook_event_name":"PreToolUse","tool_name":"Bash","tool_use_id":"toolu_bash1","tool_input":{"command":"secret-command"},"_bridge_written_at_unix":1800000000}"#;

    #[test]
    fn ask_user_question_sidecar_is_pending() {
        let p = pending_of(ASK_SIDECAR, Some(SESSION)).expect("should detect the pending question");
        assert_eq!(p.tool_use_id, "toolu_ask1");
        assert_eq!(p.kind, PromptKind::AskUserQuestion);
        assert_eq!(p.header.as_deref(), Some("Fruit"));
        assert_eq!(p.question.as_deref(), Some("Pick a fruit:"));
        assert_eq!(p.options, vec!["Apple", "Banana", "Cherry"]);
        assert_eq!(p.plan, None);
        assert_eq!(p.reaction_option_count(), 3);
        assert!(p.answerable_by_reaction());
    }

    #[test]
    fn exit_plan_mode_sidecar_is_pending() {
        let p = pending_of(PLAN_SIDECAR, Some(SESSION)).expect("should detect the pending plan");
        assert_eq!(p.tool_use_id, "toolu_plan1");
        assert_eq!(p.kind, PromptKind::ExitPlanMode);
        assert_eq!(p.header, None);
        assert!(p.plan.as_deref().unwrap().contains("Rename a variable"));
        assert_eq!(p.options.len(), 3);
        assert!(p.answerable_by_reaction());
    }

    /// The free-text third option ("Tell Claude what to change") is displayed but not
    /// reaction-eligible — see `reaction_option_count`'s doc for why. `AskUserQuestion`
    /// (tested above) is unaffected: all of its options count.
    #[test]
    fn exit_plan_mode_excludes_the_free_text_option_from_reaction_count() {
        let p = pending_of(PLAN_SIDECAR, Some(SESSION)).unwrap();
        assert_eq!(p.options.len(), 3, "still shown in full");
        assert_eq!(
            p.reaction_option_count(),
            2,
            "only the two fixed Yes options"
        );
    }

    #[test]
    fn unrelated_tool_sidecar_is_not_a_pending_prompt() {
        assert_eq!(pending_of(BASH_SIDECAR, Some(SESSION)), None);
    }

    /// Privacy guard, mirroring `status.rs`'s and `fallback_reply.rs`'s own: an unrelated
    /// tool's secret input must never surface even if somehow present in the sidecar's
    /// neighbourhood (this module only ever reads the one file, but the guard belongs here
    /// regardless — nothing about the `Bash` shape above should ever leak).
    #[test]
    fn unrelated_tool_input_never_leaks() {
        assert_eq!(pending_of(BASH_SIDECAR, Some(SESSION)), None);
    }

    #[test]
    fn missing_sidecar_is_none() {
        assert_eq!(
            read_pending_prompt_for_session_at(
                Path::new("/nonexistent/pending_prompt.json"),
                Some(SESSION),
                shortly_after(),
            ),
            None
        );
    }

    #[test]
    fn malformed_sidecar_is_none_not_a_crash() {
        assert_eq!(pending_of("not json at all", Some(SESSION)), None);
    }

    /// The core safety property: a sidecar written for a *different* session (the hook is
    /// project-scoped, so any Claude Code session sharing that project directory can write
    /// one) must never be mistaken for this bridge's own pending prompt.
    #[test]
    fn sidecar_from_a_different_session_is_ignored() {
        assert_eq!(pending_of(ASK_SIDECAR, Some("some-other-session")), None);
    }

    /// No expected session id at all (`CLAUDE_CODE_SESSION_ID` unset) must refuse to guess,
    /// the same posture `status.rs`'s own session-id resolution takes.
    #[test]
    fn no_expected_session_refuses_to_guess() {
        assert_eq!(pending_of(ASK_SIDECAR, None), None);
    }

    /// A sidecar just inside `MAX_SIDECAR_AGE` is still trusted...
    #[test]
    fn a_sidecar_within_the_age_limit_is_pending() {
        let now = UNIX_EPOCH + Duration::from_secs(WRITTEN_AT_UNIX) + MAX_SIDECAR_AGE
            - Duration::from_secs(1);
        assert!(pending_at(ASK_SIDECAR, Some(SESSION), now).is_some());
    }

    /// ...but one from a process that was killed before its `PostToolUse` half ever ran
    /// (Ctrl-C, OOM, host crash) must not resurface as "pending" indefinitely — caught in
    /// code review as a gap in the original design, which only cleared sidecars on a
    /// matching `PostToolUse` with no independent bound on how long a stale one could live.
    #[test]
    fn a_sidecar_past_the_age_limit_is_ignored() {
        let now = UNIX_EPOCH
            + Duration::from_secs(WRITTEN_AT_UNIX)
            + MAX_SIDECAR_AGE
            + Duration::from_secs(1);
        assert_eq!(pending_at(ASK_SIDECAR, Some(SESSION), now), None);
    }

    #[test]
    fn multi_select_question_is_unsupported_shape() {
        let sidecar = r#"{"session_id":"test-session-abc","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_use_id":"toolu_multi1","tool_input":{"questions":[{"question":"Pick any","header":"Multi","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}],"multiSelect":true}]},"_bridge_written_at_unix":1800000000}"#;
        let p = pending_of(sidecar, Some(SESSION))
            .expect("still a pending prompt, just not answerable");
        assert!(p.unsupported_shape);
        assert!(!p.answerable_by_reaction());
    }

    #[test]
    fn multiple_questions_array_is_unsupported_shape() {
        let sidecar = r#"{"session_id":"test-session-abc","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_use_id":"toolu_multi2","tool_input":{"questions":[{"question":"First?","header":"One","options":[{"label":"A","description":"A"}],"multiSelect":false},{"question":"Second?","header":"Two","options":[{"label":"B","description":"B"}],"multiSelect":false}]},"_bridge_written_at_unix":1800000000}"#;
        let p = pending_of(sidecar, Some(SESSION))
            .expect("still a pending prompt, just not answerable");
        assert!(p.unsupported_shape);
        assert!(!p.answerable_by_reaction());
    }

    #[test]
    fn more_than_nine_options_is_not_answerable_by_reaction() {
        let options_json: Vec<String> = (1..=10)
            .map(|n| format!(r#"{{"label":"opt{n}","description":"opt{n}"}}"#))
            .collect();
        let sidecar = format!(
            r#"{{"session_id":"test-session-abc","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_use_id":"toolu_many1","tool_input":{{"questions":[{{"question":"Pick one","header":"Many","options":[{}],"multiSelect":false}}]}},"_bridge_written_at_unix":1800000000}}"#,
            options_json.join(",")
        );
        let p = pending_of(&sidecar, Some(SESSION)).unwrap();
        assert_eq!(p.options.len(), 10);
        assert!(
            !p.unsupported_shape,
            "10 single-select options is a supported shape"
        );
        assert!(!p.answerable_by_reaction());
    }

    #[test]
    fn pending_prompt_path_honours_the_override_env_var() {
        // SAFETY-of-intent, not memory safety: `std::env::set_var` on a key unique to this
        // test (never read elsewhere in the suite) avoids the cross-test race that a shared
        // key like CLAUDE_CODE_SESSION_ID would risk under parallel test execution — see
        // `read_pending_prompt_for_session_at`'s doc for why the *session* check is designed
        // to sidestep exactly that problem instead of relying on env var isolation.
        unsafe {
            std::env::set_var(
                "CC_MATRIX_PENDING_PROMPT_PATH",
                "/tmp/cc_matrix_channel_test_pending_prompt_path_override.json",
            );
        }
        assert_eq!(
            pending_prompt_path(SESSION),
            PathBuf::from("/tmp/cc_matrix_channel_test_pending_prompt_path_override.json")
        );
        unsafe {
            std::env::remove_var("CC_MATRIX_PENDING_PROMPT_PATH");
        }
    }

    #[test]
    fn pending_prompt_path_is_namespaced_per_session_without_the_override() {
        // No override set: two different sessions must resolve to two different paths,
        // the actual fix for the sidecar-collision gap a code review caught (see
        // `pending_prompt_path`'s doc).
        unsafe {
            std::env::remove_var("CC_MATRIX_PENDING_PROMPT_PATH");
        }
        let a = pending_prompt_path("session-a");
        let b = pending_prompt_path("session-b");
        assert_ne!(a, b);
        assert!(a.to_string_lossy().contains("session-a"));
        assert!(b.to_string_lossy().contains("session-b"));
    }
}
