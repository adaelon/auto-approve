# SESSION_CHECKPOINT

> freshness: 2026-06-12T20:30+08:00
> git_head: (not a git repo, skip hash check)
> project: oly (Open Relay)

## Current State

Slice 1 of Auto-Approve feature **completed and compiling** (`cargo check` passes, 9 expected `dead_code` warnings).

Slices 2-7 remain: config integration → CLI flags → daemon startup → notification path → approval log → E2E verification.

## Frozen Intent

- **What**: Add auto-approve capability to oly daemon — when an interactive Agent is stuck at a confirmation prompt in PTY, use a cheap LLM (OpenAI-compatible API) to judge safety, auto-send input if safe, notify human if not.
- **What not**: No Web UI for approval log (future), no Anthropic native API, no retry on LLM errors.
- **Success criterion**: `cargo check` compiles; each slice has declared output and done-criteria per design doc.

## TermMap

| Term | Status | Definition |
|------|--------|------------|
| AutoApprover | NEW | Struct in `src/approval.rs` that calls LLM and returns `ApprovalDecision` |
| ApprovalDecision | NEW | Enum: `Approve{chunks}`, `Deny{reason}`, `Uncertain{reason}` |
| InputChunk | NEW | Enum: `Text(String)`, `Key(String)` — single unit of auto-sent input |
| AutoApproveConfig | NEW | Config struct: `enabled`, `api_url`, `api_key`, `model`, `max_context_lines`, `request_timeout_secs` |
| input_needed | EXISTING | Boolean flag on `SessionRuntime` — `true` when PTY appears to be waiting for input |
| screen_parser | EXISTING | `vt100::Parser` on `SessionRuntime` holding current terminal screen state |
| try_write_input | EXISTING | Method on `PtyHandle` — non-blocking send of bytes to PTY stdin |
| silent_candidates | EXISTING | Method on `SessionStore` returning sessions that may need input |
| run_notification_monitor | EXISTING | Async loop in `notification/mod.rs` that detects input_needed and dispatches notifications |

## ChangeType

[model-extension] — adding a new subsystem (auto-approve) that plugs into existing notification path.

## Pitfalls & Correct Practices

### P1: PowerShell Set-Content corrupts UTF-8 multi-byte characters

**What happened**: Used `(Get-Content file -Raw) -replace ... | Set-Content file -Encoding UTF8` on `main.rs`. PowerShell's `-replace` + `Set-Content -Encoding UTF8` corrupted em-dash `—` (U+2014, 3-byte UTF-8) into garbled `鈥?`, breaking Rust compilation.

**Root cause**: `Set-Content -Encoding UTF8` writes UTF-8 with BOM on older PowerShell, and string round-tripping through `-replace` can mangle multi-byte sequences when the file was originally UTF-8 without BOM.

**Correct practice for in-place edits of Rust/UTF-8 source files**:

```powershell
# SAFE: .NET API, explicit UTF-8 no BOM
$path = 'E:\...\file.rs'
$content = [System.IO.File]::ReadAllText($path, [System.Text.Encoding]::UTF8)
$content = $content.Replace('old', 'new')
[System.IO.File]::WriteAllText($path, $content, [System.Text.UTF8Encoding]::new($false))
```

**Alternative**: Use `apply_patch` (the Codex CLI built-in tool) for targeted line edits — it handles encoding correctly. Reserve PowerShell file operations for **creating new files** (here-strings with `Set-Content` are fine for new files since there's no prior encoding to corrupt).

### P2: Rust on Windows requires MSVC Build Tools

**What happened**: `cargo check` failed with `link.exe not found` even though `rustc` and `cargo` were installed.

**Fix**: Install "Desktop development with C++" workload from Visual Studio Build Tools. After install, `cargo` works from a fresh terminal.

### P3: reqwest needs json feature for .json() method

**What happened**: `self.client.post(&url).json(&body)` in `approval.rs:123` failed with `no method named json found for struct RequestBuilder`.

**Fix**: Add `"json"` to reqwest features in `Cargo.toml`:
```toml
reqwest = { version = "0.13", default-features = false, features = ["rustls", "stream", "json"] }
```

### P4: RustEmbed requires web/dist to exist at compile time

**What happened**: `#[derive(RustEmbed)]` with `folder = "web/dist"` caused compile error when directory didn't exist.

**Fix**: Create empty `web/dist/` directory. This is a pre-existing project requirement unrelated to our changes.

## Code Trail

| Slice | Files Changed | Status |
|-------|---------------|--------|
| S1 | `src/approval.rs` (new), `src/main.rs` (+1 line `mod approval;`), `Cargo.toml` (+json feature), `web/dist/` (created dir) | ✅ done |

## Architecture Impact

No boundary changes yet. `approval.rs` is a standalone module with no callers. Slices 2-5 will wire it into config, CLI, daemon lifecycle, and notification monitor.

## Uncommitted Changes

- `src/approval.rs` — new file (AutoApprover + types + tests)
- `src/main.rs` — added `mod approval;`, fixed 3 corrupted em-dash lines
- `Cargo.toml` — added `json` feature to reqwest
- `web/dist/` — created empty directory (WAS already needed)
- `src/approval.rs` is 0 bytes in git (was pre-created empty), now populated

## Next Steps (atomic actions)

1. Slice 2 — Config integration: edit `src/config.rs`, add `auto_approve: AutoApproveConfig` field to `AppConfig`, parse from `config.json` + env `OLY_AUTO_APPROVE_API_KEY`, update `AppConfigOverrides` and `with_runtime_overrides()`. Done criterion: `cargo check` passes, `AppConfig::load()` reads `auto_approve` section.
2. Slice 3 — CLI flags: edit `src/cli.rs`, add `--auto-approve`, `--auto-approve-api-url`, `--auto-approve-api-key`, `--auto-approve-model` to `DaemonStartArgs`. Done criterion: `cargo check` passes, `oly daemon start --help` shows new flags.
3. Slice 4 — Daemon startup integration: edit `src/daemon/lifecycle.rs`, construct `AutoApprover` from config, pass into notification monitor. Edit `src/main.rs` to pass CLI flags. Done criterion: `cargo check` passes, `AutoApprover` is constructed at startup.
4. Slice 5 — Notification path integration: edit `src/notification/mod.rs`, call `AutoApprover::judge()` when `input_needed` becomes true, handle `Approve/Deny/Uncertain`. Done criterion: manual test shows LLM judgment flow.
5. Slice 6 — Approval log: append each decision to `sessions/<id>/approval.log`. Done criterion: approval creates log file.
6. Slice 7 — E2E verification. Done criterion: demo passes.

## Cold-start Reading Sequence

1. `docs/auto-approve-design.md` — full technical design
2. `docs/auto-approve-slices.md` — slice definitions
3. `src/approval.rs` — completed Slice 1
4. `src/notification/mod.rs` — integration point (future Slice 5)
5. `src/config.rs` — next target (Slice 2)
6. `src/cli.rs` — future Slice 3
7. `src/daemon/lifecycle.rs` — future Slice 4
