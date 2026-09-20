# 0001 - Rust with Axum

## Status

Accepted

## Context

underclass is a long-running network proxy that fans concurrent streaming (SSE) requests out over a pool of upstream subscriptions. It must handle hundreds of concurrent streams with low overhead, ship as a single deployable artifact, and fit the existing devenv toolchain in this repository (`languages.rust.enable = true`).

Candidates considered: Go (single binary, strong concurrency), TypeScript + Bun/Hono (shares idioms with the opencode codebase we modeled against), Rust with Axum, Rust with other frameworks.

## Decision

Implement in Rust using Axum on Tokio, with reqwest for upstream calls.

## Consequences

- Single static binary; excellent streaming and concurrency characteristics via tokio tasks and `bytes_stream` passthrough.
- The devenv shell already provides the Rust toolchain; no new environment requirements.
- Slower iteration than TypeScript for UI work; mitigated by keeping the web UI as a single embedded HTML file.
- Cargo is the only build system; `cargo build` / `cargo test` are the canonical commands.
