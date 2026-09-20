# 0008 - JSONC-Safe Config Merge for `underclass connect`

## Status

Accepted

## Context

`underclass connect` writes the `provider.underclass` block into the user's existing opencode config. opencode reads `opencode.json` and `opencode.jsonc` (preferring `.jsonc` when both exist) and parses them as JSONC: comments and trailing commas are legal. Users' configs are hand-maintained and precious.

## Decision

- Locate the config in opencode's own preference order: `~/.config/opencode/opencode.jsonc`, then `opencode.json` (or `./.opencode/opencode.json` with `--project`).
- Parse with comment stripping plus a trailing-comma sanitizer before `serde_json` parsing.
- Merge only the `provider.underclass` key and the `model` key (only when it points at `underclass/*`); all unrelated keys are preserved byte-for-value.
- Always write a `.bak` backup before overwriting. Merging is idempotent: `connect; connect` produces the same file.
- `--remove` undoes exactly what `connect` wrote (provider block, `model` if it references `underclass`, and the `auth.json` credential).
- Credentials go to `~/.local/share/opencode/auth.json` as `{ "underclass": { "type": "api", "key": ... } }`, created with `0600`.

## Consequences

- Comments in a user's config survive parsing but are dropped on rewrite (the file is re-serialized as pretty JSON). The `.bak` file preserves the original. This trade-off was accepted over shipping a comment-preserving JSONC editor.
- Property-backed guarantees: merge idempotency and remove-undoes-connect are enforced by Hegel-style property and unit tests.
