# 0002 - Property-Based Testing with Hegel, No Mutation Testing

## Status

Accepted

## Context

The pool core (routing, stickiness, health state machine) is invariant-heavy logic where example-based tests under-specify behavior: off-by-one errors in TTL handling, cooldown comparisons, and least-in-flight selection are exactly the bugs that hand-written cases miss.

Mutation testing (cargo-mutants) was considered as a way to measure test strength. Property-based testing (PBT) generates inputs to directly attack the invariants themselves.

## Decision

Use [Hegel](https://hegel.dev) (`hegeltest`, version pinned in Cargo.toml) as the property-based testing framework. Testing strategy is two-tier:

1. Plain `#[test]` unit tests for exact behavior (header wiring, URL rewriting, JSONC merging, redaction).
2. Hegel property tests (`tests/properties.rs`) for invariants: stickiness stability, state-machine health, saturation minimums, TTL/cap bounds.

Mutation testing was considered and **rejected**: strong property tests already kill the mutant classes mutation testing targets, while adding significant CI time (one full test run per mutant).

## Consequences

- The pool/health logic lives in a pure, synchronous `PoolCore` with an injected clock so generated sequences and a virtual timeline can drive it deterministically from `#[hegel::test]` functions. Async code exists only at the edges (axum handlers, reqwest, device-flow polling).
- `hegeltest` is pinned; it is beta software and may make breaking changes (see ADR references in AGENTS.md).
- New pure-logic modules are expected to ship with properties, not just unit tests.
- Surviving property failures indicate real contract questions - resolve by fixing code or tightening the property, never by loosening assertions.
