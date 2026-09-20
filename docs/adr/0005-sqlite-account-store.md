# 0005 - SQLite for Account and Binding Storage

## Status

Accepted

## Context

The proxy persists OAuth credentials (rotating refresh tokens), per-account health state, sticky bindings, and the model catalog. It runs as a single process on a user's machine; deployment targets are developer laptops and small servers.

## Decision

Use SQLite (rusqlite, bundled) with a single database file at `~/.local/share/underclass/pool.db`. Tables: `accounts`, `bindings`, `catalog`, `config`. Access is behind a mutex; writes are write-through for tokens, health state, bindings, and catalog.

## Consequences

- Zero-configuration persistence; no external services.
- Rotated refresh tokens are persisted immediately on every successful refresh - losing them would invalidate the subscription.
- Single-process assumption: concurrent writers from multiple `underclass serve` instances on the same data dir are not supported.
- Tokens rest in plaintext in the database file; the file's permissions and host security are the control. This is documented as an accepted risk for a local developer tool.
