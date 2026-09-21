# 0013 - Pooled Subscription Usage

## Status

Accepted

## Context

The Codex subscription service exposes plan and rate-limit windows for each ChatGPT account at its usage endpoint. Operators need these limits beside each configured account and clients can consume the same native response shape from the proxy. An account can omit a window, fail authentication, use an unsupported backend, or return usage for a different window duration. Those cases cannot be treated as unused capacity.

## Decision

Fetch Codex usage with credentials obtained from `TokenManager` and cache the sanitized plan and window fields for one minute. The admin state reports an explicit available, unavailable, or pending state for every account. Unsupported and disabled accounts remain visible and are unavailable for pooled usage.

A fresh exhausted sample with known reset timestamps moves the account to cooling until the latest exhausted window resets. This timestamp replaces a fallback cooldown learned from an inference response. Missing, failed, stale, or reset-free samples do not change routing health.

Expose the pooled result at `/v1/usage`, `/v1/api/codex/usage`, `/api/codex/usage`, and `/backend-api/wham/usage`, protected by the proxy API key. The response uses the native Codex usage shape and adds `_underclass` metadata for aggregation method, coverage, and staleness.

Aggregate matching window durations across both native window positions. Each reporting account has equal weight because the upstream service does not publish an absolute allowance. Missing samples do not contribute zero. Round the mean used percentage upward and use the earliest reset among reporting accounts. The pool is allowed when at least one eligible account's upstream usage status is allowed and has not reached its limit.

## Consequences

- The dashboard shows individual account usage and an explicitly labeled equal-account pooled estimate.
- Mixed plans and partial upstream failures reduce reported coverage instead of increasing apparent remaining capacity.
- Clients that call the Codex usage HTTP endpoint can parse the pooled response. Clients that reject API-key authentication before making a usage request require a client-side change.
- Telemetry refresh failures do not change account health or pool routing.
