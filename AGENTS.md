You can get Rust build methods from `build_manual.md`.

## CRITICAL: Do Not Change Design Without Permission

- NEVER change product design/UX without explicit user request.
- Bug fix and design change are different tasks.
- If you identify a "potential improvement" or "UX issue", only report it.
- When user says "fix it", fix only the bugs requested.
- Ask first before any design-level changes.

## Build And Test Guidelines

- Only run build/test commands when the user explicitly requests them.
- Do not run `cargo build`, `cargo test`, or `python3 build.py` automatically.
- Focus on minimal, targeted code changes.

## Safety Guidelines

- Prefer non-destructive, non-interactive commands.
- Never run destructive commands such as `rm -rf`.
- Never open interactive editors from automation commands.
- Never commit or print secrets (API keys, tokens, private credentials).

## AI Backend Guidelines (Codex CLI)

- This repository uses OpenAI Codex CLI for AI features.
- Check availability with `which codex` (fallback: `bash -lc "which codex"`).
- Stream events using `codex exec --json` and parse JSONL safely.
- Resume sessions with `codex exec resume <SESSION_ID>` using stored `thread_id`.
- Keep AI backend changes minimal and avoid unrelated refactors.

## Version Management

- Version is defined in `Cargo.toml` (`version = "x.x.x"`).
- Version display uses `env!("CARGO_PKG_VERSION")`.
- To update version, modify only `Cargo.toml`.
- Never hardcode version strings in source files.

## Theme Color System

- All color definitions must use `Color::Indexed(number)` directly.
- Each UI element needs its own uniquely named color field.
- Do not reference another element's color field by alias.
- Add dedicated color fields in the appropriate `*Colors` struct.

### Theme File Locations

- Source of truth: `src/ui/theme.rs`.
- Generated files: `~/.cokacdir/themes/*.json`.
- Do not edit generated theme JSON directly.
- JSON comments are defined in `src/ui/theme.rs` (`to_json()`).
