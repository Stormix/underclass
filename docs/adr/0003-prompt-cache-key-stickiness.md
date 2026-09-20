# 0003 - Sticky Routing by Prompt Cache Key

## Status

Accepted

## Context

Both Codex (`chatgpt.com/backend-api/codex/responses`) and GitHub Copilot serve OpenAI-family models with server-side prompt caching keyed to the account that issued earlier turns of a conversation. Routing consecutive turns of the same session to different accounts both forfeits that cache and leaks conversation context across subscriptions.

opencode (verified in its source, `provider/transform.ts`) sends `promptCacheKey` (snake_case `prompt_cache_key` for some SDKs) in the request body, set to the session ID, when the client is configured with `setCacheKey: true`. It also sends a `session-id` header on its native OpenAI path.

## Decision

- Extract the stickiness key from the request body (`prompt_cache_key`, then `promptCacheKey`), falling back to the `session-id` header. Requests with no extractable key are routed unsticky (least-in-flight) and never bound.
- Maintain a sticky map `cache_key -> (backend, account)` as an LRU cache (cap 10,000 entries) with a 24-hour TTL.
- A sticky hit whose account is still healthy and serves the model reuses that account. If the bound account is cooling/unhealthy, rebind preferring the **same backend** (keeps the upstream cache warm), falling back cross-backend only when no same-backend account is healthy.
- Bindings are persisted to SQLite so stickiness survives proxy restarts.

## Consequences

- opencode clients must set `setCacheKey: true` (the generated `underclass connect` snippet does).
- Rebinding mid-session loses the upstream prompt cache for that session; this is unavoidable when the bound account is out of quota and is strictly better than failing the request.
- The 24h TTL bounds memory; long-lived sessions rebind silently after expiry.
