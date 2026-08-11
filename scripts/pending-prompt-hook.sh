#!/usr/bin/env bash
# PreToolUse/PostToolUse hook for AskUserQuestion/ExitPlanMode: writes (and clears) the
# sidecar file src/pending_prompt.rs reads instead of the transcript.
#
# Why this exists: confirmed live (tools/menu-spike/FINDINGS.md) that the transcript
# .jsonl never gets the tool_use record for these two tools until they're *resolved* — so
# a transcript-only detector can never see one while it's actually blocking the terminal.
# A PreToolUse hook does fire before the block, with the full tool_input already attached,
# confirmed the same way. This script is that hook's command.
#
# Wire into settings.json (project-local `.claude/settings.json` in the session's cwd, or
# user `~/.claude/settings.json` — NOT done automatically by anything in this repo; see
# README for the snippet and why deploying it to the live bridge session is a separate,
# deliberate step):
#
#   {
#     "hooks": {
#       "PreToolUse": [{"matcher": "AskUserQuestion|ExitPlanMode",
#                        "hooks": [{"type": "command", "command": "<path-to-this-script>"}]}],
#       "PostToolUse": [{"matcher": "AskUserQuestion|ExitPlanMode",
#                         "hooks": [{"type": "command", "command": "<path-to-this-script>"}]}]
#     }
#   }
#
# Claude Code passes the hook payload as JSON on stdin, including hook_event_name,
# tool_name, tool_input, tool_use_id and session_id — see tools/menu-spike/
# pretooluse-probe.log for real captured examples of the exact shape.
#
# Requires jq — present in this environment (base image, not apt-installed this session)
# and used rather than sed/grep field-scraping specifically because tool_input for
# ExitPlanMode carries arbitrary plan markdown; a real parser is the only way to pull
# tool_name/hook_event_name/tool_use_id out safely regardless of what that field contains.
#
# The matcher restricts which tool names invoke this at all, but it's re-checked here too
# (defense in depth, and the matcher could be misconfigured as "*" for other reasons).
#
# Sidecar is namespaced per session_id (pending_prompt-<session_id>.json), NOT one shared
# path. A code review caught that a shared path meant this hook — configured at project or
# user scope, so it fires for *every* Claude Code session sharing that scope, not just the
# bridge's own — could have a second, unrelated session's PreToolUse `mv -f` right over the
# bridge's own still-open entry. The old design's session_id check in
# src/pending_prompt.rs only protected the *read* side (never misreading a foreign entry as
# the bridge's own); it did nothing to stop the foreign *write* from clobbering the
# bridge's genuinely-pending one, which would have made the bridge see "resolved" and post
# that to Matrix while the terminal was still blocked. Giving every session its own file
# removes the collision instead of just detecting it after the fact — this script computes
# the same filename from the payload's own session_id that src/pending_prompt.rs computes
# from CLAUDE_CODE_SESSION_ID, with no coordination needed between the two.
#
# CC_MATRIX_PENDING_PROMPT_PATH, if set, overrides the path outright (used by tests that
# want an isolated file without a real session id) — same env var name and semantics
# src/pending_prompt.rs's `pending_prompt_path` uses, so the two stay in sync.

set -uo pipefail

payload="$(cat -)"

# Malformed/non-JSON stdin: do nothing rather than guess or crash the hook.
if ! printf '%s' "$payload" | jq -e . >/dev/null 2>&1; then
    exit 0
fi

tool_name="$(printf '%s' "$payload" | jq -r '.tool_name // empty')"
case "$tool_name" in
    AskUserQuestion | ExitPlanMode) ;;
    *) exit 0 ;;
esac

session_id="$(printf '%s' "$payload" | jq -r '.session_id // empty')"
if [ -z "$session_id" ] && [ -z "${CC_MATRIX_PENDING_PROMPT_PATH:-}" ]; then
    # No session id and no override to fall back on — nothing safe to name the file.
    exit 0
fi

SIDECAR="${CC_MATRIX_PENDING_PROMPT_PATH:-$HOME/.claude/channels/matrix/pending_prompt-${session_id}.json}"

event="$(printf '%s' "$payload" | jq -r '.hook_event_name // empty')"

mkdir -p "$(dirname "$SIDECAR")"

case "$event" in
    PreToolUse)
        # Stamp a write timestamp so src/pending_prompt.rs can reject a stale sidecar
        # (MAX_SIDECAR_AGE) — a process killed mid-prompt (Ctrl-C, OOM, host crash) before
        # its PostToolUse half ever ran would otherwise leave an orphaned entry with no
        # bound on how long it could resurface as "pending." Field name prefixed `_bridge_`
        # since Claude Code doesn't write it — this script adds it before saving.
        stamped="$(printf '%s' "$payload" | jq --argjson ts "$(date +%s)" '. + {_bridge_written_at_unix: $ts}')"
        # Atomic write: a temp file on the same filesystem, then rename — the bridge
        # polls this file on its own tick and must never observe a half-written one.
        tmp="$(mktemp "${SIDECAR}.XXXXXX")"
        printf '%s' "$stamped" >"$tmp"
        mv -f "$tmp" "$SIDECAR"
        ;;
    PostToolUse)
        # Only clear if the sidecar's own tool_use_id matches this resolution — a stale
        # PostToolUse for an already-superseded entry must not delete a newer pending one.
        this_id="$(printf '%s' "$payload" | jq -r '.tool_use_id // empty')"
        if [ -f "$SIDECAR" ]; then
            current_id="$(jq -r '.tool_use_id // empty' "$SIDECAR" 2>/dev/null || true)"
            if [ "$current_id" = "$this_id" ]; then
                rm -f "$SIDECAR"
            fi
        fi
        ;;
esac

exit 0
