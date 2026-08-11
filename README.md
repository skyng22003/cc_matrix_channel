[![CI](https://github.com/IA-PieroCV/cc_matrix_channel/actions/workflows/ci.yml/badge.svg)](https://github.com/IA-PieroCV/cc_matrix_channel/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

# Matrix Channel for Claude Code

Chat with your running Claude Code session from any Matrix client.

Works with any Matrix homeserver (Continuwuity, Synapse, Conduit, Dendrite). Full E2EE support.

## Features

- Two-way messaging with reply threading
- File attachments (send, receive, auto-decrypt E2EE media)
- Permission relay — approve/deny tool calls remotely
- Runtime config — change settings without restart
- Pairing-based access control

## Setup (one-time)

Requires [Bun](https://bun.sh).

```
/plugin marketplace add IA-PieroCV/cc_matrix_channel
/plugin install matrix@cc-matrix-channel
/reload-plugins
# You must need to restart your session to get the slash commands working and connect to the channel MCP
# claude --dangerously-load-development-channels plugin:matrix@cc-matrix-channel
/configure https://matrix.example.com @bot:example.com YOUR_PASSWORD
OR
/matrix:configure https://matrix.example.com @bot:example.com YOUR_PASSWORD
```

Then pair your account — DM the bot from your Matrix client:

```
/access pair <code>
OR
/matrix:access pair <code>

/access policy allowlist
OR
/matrix:access policy allowlist
```

## Usage (every session)

```bash
claude --dangerously-load-development-channels plugin:matrix@cc-matrix-channel
```

## Skills

| Skill | Description |
|-------|-------------|
| `/matrix:access` | Pairing, allowlists, groups, delivery settings |
| `/matrix:configure` | Credentials, status, setup guide |

## Tools

| Tool | Description |
|------|-------------|
| `reply` | Send text + optional files, with threading |
| `react` | Emoji reaction |
| `edit_message` | Edit a sent message |
| `download_attachment` | Download files (auto-decrypts E2EE) |
| `send_attachment` | Send local files |
| `fetch_messages` | Room history |
| `approve_pairing` | Approve user access (terminal only) |

## Configuration

### Connection (`~/.claude/channels/matrix/.env`)

| Variable | Required | Description |
|---|---|---|
| `MATRIX_HOMESERVER_URL` | Yes | Homeserver URL |
| `MATRIX_USER_ID` | Yes | Bot user ID (`@bot:server`) |
| `MATRIX_PASSWORD` | Yes* | Bot password (first-run only) |
| `MATRIX_STORE_PATH` | No | E2EE key storage (default: `./data/matrix_store`) |
| `MATRIX_ACCESS_TOKEN` | No | Fallback auth (limited E2EE) |
| `MATRIX_DEVICE_ID` | No | Device ID hint (auto-generated) |
| `MATRIX_STORE_PASSPHRASE` | No | Encrypt store at rest |

### Access & Delivery (`~/.claude/channels/matrix/access.json`)

Managed via `/matrix:access`. Changes take effect immediately.

| Setting | Default | Description |
|---------|---------|-------------|
| `dmPolicy` | `pairing` | `pairing`, `allowlist`, or `disabled` |
| `allowFrom` | `[]` | Allowed user IDs |
| `groups` | `{}` | Per-room mention-only + patterns |
| `ackReaction` | `👀` | Ack emoji (empty to disable) |
| `textChunkLimit` | `4096` | Max chars per chunk |
| `chunkMode` | `newline` | `newline` or `length` |
| `replyToMode` | `first` | `first`, `all`, or `off` |

### Menu forwarding (`AskUserQuestion`/`ExitPlanMode` over Matrix)

The bridge can forward Claude Code's own blocking CLI prompts to Matrix and let you answer
by tapping a number-emoji reaction, instead of the session hanging until someone attaches
via `tmux`. Detection needs no bridge config beyond the two env vars below — it works by
reading a sidecar file that a `PreToolUse`/`PostToolUse` hook writes, **not** the
transcript (the transcript does not carry these tool calls until they're already resolved
— confirmed live, see `tools/menu-spike/FINDINGS.md`).

**Wiring the hook in** (not done automatically — a deliberate, separate step): add to the
session's `settings.json` (project-local `.claude/settings.json` in its cwd, or
`~/.claude/settings.json` for the whole user):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "AskUserQuestion|ExitPlanMode",
        "hooks": [{ "type": "command", "command": "/path/to/cc_matrix_channel/scripts/pending-prompt-hook.sh" }]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "AskUserQuestion|ExitPlanMode",
        "hooks": [{ "type": "command", "command": "/path/to/cc_matrix_channel/scripts/pending-prompt-hook.sh" }]
      }
    ]
  }
}
```

Because a project-level hook fires for *every* Claude Code session sharing that project
directory, not just the bridge's own, the sidecar carries `session_id` and the bridge
checks it against its own `CLAUDE_CODE_SESSION_ID` before trusting it — a foreign
session's entry reads as if there were no pending prompt at all.

**Answering beyond a plain numbered choice** — confirmed live, see
`tools/menu-spike/FINDINGS.md`'s options-4/5/3 section:

- **❌ decline** — react with ❌ (pre-seeded alongside the numbered options) to reject the
  prompt outright, submitted blank. For `AskUserQuestion` this hits the CLI's own "Type
  something." entry (its other trailing option, "Chat about this", also declines but
  additionally makes Claude auto-continue with a clarifying question — not wired up
  separately, since a decline plus a real reply already covers it); for `ExitPlanMode` it's
  "Tell Claude what to change" with nothing typed.
- **Reply with text** — reply (not just send a new message) to the prompt message with
  your own text, and the bridge types it into that same free-text option instead of
  submitting it blank:
  - `ExitPlanMode`: submits with `shift+tab` instead of `Enter` — **approves the plan and
    attaches your text as feedback in the same turn**, not a reject-then-retry round trip.
  - `AskUserQuestion`: submits with plain `Enter` — **your text becomes Claude's actual
    answer** to the question, the same as picking a real option.
- **💬 "chat about this"** — `AskUserQuestion` only (pre-seeded alongside ❌ there, not
  offered on `ExitPlanMode` prompts). Also declines, but Claude auto-continues with a
  clarifying question instead of stopping silently — the difference from ❌: you don't have
  to know to follow up, Claude asks first.

| Variable | Required | Description |
|---|---|---|
| `CC_MATRIX_PENDING_PROMPT_PATH` | No | Overrides the sidecar path outright (default: `~/.claude/channels/matrix/pending_prompt-<session_id>.json`, namespaced per session so a second Claude Code session sharing the hook's scope can't clobber the bridge's own pending entry) — if set, must match between the hook script and the bridge process |
| `CC_MATRIX_TMUX_PANE` | No | tmux target for answering (default: `claude-matrix:claude-code`) |
| `CC_MATRIX_TMUX_ANSWERS_ENABLED` | No | Kill switch for the answer-relay keystroke path (default: `false` — detection/posting to Matrix always runs regardless) |

Requires `jq` (the hook script's own dependency).

## Manual Install

Without the plugin system:

1. Download binary from [Releases](https://github.com/IA-PieroCV/cc_matrix_channel/releases/latest)
2. Save credentials to `~/.claude/channels/matrix/.env`
3. Add to `.mcp.json`:
   ```json
   { "mcpServers": { "matrix": { "command": "/path/to/cc_matrix_channel" } } }
   ```
4. `claude --dangerously-load-development-channels server:matrix`

## Build from Source

```bash
git clone https://github.com/IA-PieroCV/cc_matrix_channel
cd cc_matrix_channel
cargo build --release   # Requires Rust 1.85+
```

## Testing / Development

### Test an RC build with `--plugin-dir`

The `dev` branch produces pre-release binaries. The Bun launcher downloads the exact version listed in `plugin.json` directly from GitHub Releases, so RC builds work the same as stable ones:

```bash
git clone https://github.com/IA-PieroCV/cc_matrix_channel
cd cc_matrix_channel
git checkout dev
# In your terminal:
claude --dangerously-load-development-channels --plugin-dir /path/to/cc_matrix_channel plugin:matrix@cc-matrix-channel
```

The launcher fetches the RC binary on first run and caches it.

### Test local code changes (no GitHub release needed)

Build the binary and drop it in the launcher's cache path — the launcher skips the download when the file already exists:

```bash
cargo build --release
VERSION=$(grep '^version' .claude-plugin/plugin.json | sed 's/.*"\(.*\)".*/\1/')
cp target/release/cc_matrix_channel \
   ~/.claude/channels/matrix/plugin-data/bin/cc_matrix_channel-v${VERSION}
chmod +x ~/.claude/channels/matrix/plugin-data/bin/cc_matrix_channel-v${VERSION}
```

Then run with `--plugin-dir` as above. Repeat the `cp` after each `cargo build`.

## Troubleshooting

| Problem | Fix |
|---|---|
| E2EE fails | Delete store directory, restart with password |
| "Cannot send to this room" | Send a message to the bot first |
| Bot ignores messages | Check `/matrix:access` or complete pairing |
| Session won't restore | Verify `MATRIX_STORE_PATH` |

---

Licensed under [MIT](LICENSE).
