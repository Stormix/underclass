# 0009 - Device-Code Onboarding for Multi-Account Setup

## Status

Accepted

## Context

The proxy's value comes from many subscriptions in one pool. Account onboarding must work headless (SSH, containers) and allow authorizing N accounts in sequence from one browser.

Two upstreams are supported, each with its own grant:

- **Codex (ChatGPT)**: OAuth device flow at `auth.openai.com` (`POST /api/accounts/deviceauth/usercode` -> user enters code at `auth.openai.com/codex/device` -> poll `POST /api/accounts/deviceauth/token` -> exchange at `/oauth/token`). Client ID `app_EMoamEEZ73f0CkXaXp7hrann`, as used by the opencode Codex plugin.
- **GitHub Copilot**: GitHub OAuth device flow (`POST github.com/login/device/code`, client ID `Ov23li8tweQw6odWQebz`, scope `read:user`, RFC 8628 `authorization_pending`/`slow_down` polling), with optional GitHub Enterprise domain per account.

Browser-PKCE-on-the-server was considered and rejected: it requires a browser on the host and a localhost callback per account, which is hostile to headless pool assembly.

## Decision

The web UI drives the device flow: it displays the user code and verification URL, and polls flow status until the account appears. Each flow can also target an existing `AuthError` account for re-login (token replacement, same account identity).

## Consequences

- Adding 20 accounts is 20 device authorizations from one browser tab; no browser is needed on the proxy host itself.
- Refresh tokens rotate on every Codex refresh; the latest token is always persisted (ADR 0005).
- Copilot tokens are long-lived (`expires: 0` semantics): no refresh flow exists, and 401 moves the account to `AuthError`.
