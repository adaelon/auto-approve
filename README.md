# oly — auto-approve

`oly` manages detached PTY sessions (shells, coding-agent CLIs like Claude Code
and Codex) behind a daemon, and can notify a human when a session is waiting for
input. **Auto-approve** lets a cheap LLM stand in for that human on routine
confirmation prompts, so long-running agent tasks keep moving without manual
clicks.

## How auto-approve works

The notification monitor runs once per second. On every tick it scans each
running session's live screen (independent of the silence-based human
notification path) and, for any screen that is a **numbered confirmation menu**,
asks the configured LLM whether the pending action is safe:

```
Do you want to proceed?
❯ 1. Yes
  2. Yes, and allow ... (always)
  3. No
```

1. **Confirmation gate** — the ANSI-stripped screen must contain a `N. Yes`
   option (e.g. `❯ 1. Yes`). Idle input prompts, slash-command autocompletes and
   ordinary output never trigger the judge.
2. **LLM judge** — the screen is sent to an OpenAI-compatible
   `/v1/chat/completions` endpoint. The model replies on a single line:
   `APPROVE: <n>` (pick the plain "Yes" option, not "always allow"),
   `DENY: <reason>`, or `UNCERTAIN: <reason>`.
3. **Action** — on `APPROVE`, the chosen input is written to the PTY and the
   human notification for that screen is suppressed. `DENY`/`UNCERTAIN` defer to
   the human. Every decision is appended to `sessions/<id>/approval.log`.
4. **Cooldown** — after a judgement a session is not re-judged for ~6s, so an
   animated/blinking prompt is not approved repeatedly.

Because the scan looks at the live screen rather than waiting for output to go
quiet, it works with coding-agent TUIs that continuously redraw (spinners,
blinking cursors) and therefore never appear "silent".

## Configuration

Auto-approve is configured in `%LOCALAPPDATA%\oly\config.json` (Windows) /
`~/.local/share/oly/config.json` (Linux/macOS):

```json
{
  "auto_approve": {
    "enabled": true,
    "api_url": "https://your-openai-compatible-endpoint/v1/",
    "api_key": "sk-...",
    "model": "your-model",
    "max_context_lines": 80,
    "request_timeout_secs": 10
  }
}
```

The API key may also be supplied out-of-band via the `OLY_AUTO_APPROVE_API_KEY`
environment variable, which takes precedence over the config file. It is passed
to the daemon process by environment, never as a CLI argument (so it does not
appear in process listings).

## Running coding agents inside oly

The daemon spawns each session with the daemon's own environment. Two details
matter when running `claude` (Claude Code) inside an oly session:

- **Network egress** — `claude` talks to Anthropic directly. If you reach the
  API through a local proxy, the proxy variables must be present in the
  environment that starts the daemon, or the session inherits no proxy and the
  request is rejected:

  ```powershell
  $env:HTTP_PROXY  = "http://127.0.0.1:10809"
  $env:HTTPS_PROXY = "http://127.0.0.1:10809"
  oly daemon start
  ```

- **Nested-session markers** — oly strips `CLAUDECODE`, `CLAUDE_CODE_*` and a
  Claude-Code `AI_AGENT` value from spawned sessions so a nested `claude` does
  not mistake itself for a child of an outer agent (which would break its OAuth
  flow). `CLAUDE_CODE_OAUTH_TOKEN` is explicitly passed through, so you can
  inject a token from `claude setup-token` if you prefer a static credential.

## Logs

- Daemon log: `%LOCALAPPDATA%\oly\logs\daemon.log.<date>`
  (set `RUST_LOG=oly=debug` for the per-tick auto-approve trace).
- Per-session approval decisions: `sessions/<id>/approval.log` (one JSON line
  per decision).
