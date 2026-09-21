use crate::models::{Account, AccountStatus, BackendId, now_ms};
use crate::store::Store;
use crate::tokens::TokenManager;
use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex as SyncMutex};
use tokio::sync::Mutex;

const REFRESH_INTERVAL_MS: i64 = 60_000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct UsageWindow {
    pub used_percent: f64,
    pub limit_window_seconds: i64,
    pub reset_after_seconds: i64,
    pub reset_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountUsage {
    pub plan_type: String,
    pub allowed: bool,
    pub limit_reached: bool,
    pub primary_window: Option<UsageWindow>,
    pub secondary_window: Option<UsageWindow>,
    pub fetched_at_ms: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AccountUsageState {
    Available { usage: AccountUsage },
    Unavailable { reason: String, checked_at_ms: i64 },
    Pending,
}

#[derive(Default)]
struct Cache {
    refreshed_at_ms: i64,
    accounts: HashMap<String, AccountUsageState>,
}

pub struct UsageService {
    store: Arc<Store>,
    tokens: Arc<TokenManager>,
    client: reqwest::Client,
    pool: Arc<SyncMutex<crate::pool::PoolCore>>,
    cache: Mutex<Cache>,
    refresh_lock: Mutex<()>,
}

impl UsageService {
    pub fn new(
        store: Arc<Store>,
        tokens: Arc<TokenManager>,
        client: reqwest::Client,
        pool: Arc<SyncMutex<crate::pool::PoolCore>>,
    ) -> Self {
        Self {
            store,
            tokens,
            client,
            pool,
            cache: Mutex::new(Cache::default()),
            refresh_lock: Mutex::new(()),
        }
    }

    pub async fn snapshot(&self) -> HashMap<String, AccountUsageState> {
        self.cache.lock().await.accounts.clone()
    }

    pub async fn authoritative_reset_ms(&self, account_id: &str, current_ms: i64) -> Option<i64> {
        let cache = self.cache.lock().await;
        let AccountUsageState::Available { usage } = cache.accounts.get(account_id)? else {
            return None;
        };
        if current_ms.saturating_sub(usage.fetched_at_ms) > REFRESH_INTERVAL_MS * 2 {
            return None;
        }
        exhausted_reset_ms(usage, current_ms)
    }

    /// @cc [owner:ghuntley,label:usage;security] usage-refresh-safe-partial-results
    /// Refreshing subscription usage MUST obtain credentials through `TokenManager`, MUST NOT
    /// retain or return credentials or upstream error bodies, and MUST record each failed or
    /// unsupported account as unavailable rather than as zero usage.
    pub async fn refresh(&self) {
        let _refresh_guard = self.refresh_lock.lock().await;
        {
            let cache = self.cache.lock().await;
            if now_ms().saturating_sub(cache.refreshed_at_ms) < REFRESH_INTERVAL_MS {
                return;
            }
        }
        let accounts: Vec<Account> = self.store.list_accounts();
        let results = join_all(accounts.into_iter().map(|account| async move {
            let id = account.id.clone();
            tokio::time::timeout(
                std::time::Duration::from_secs(15),
                self.fetch_account(account),
            )
            .await
            .unwrap_or_else(|_| {
                (
                    id,
                    AccountUsageState::Unavailable {
                        reason: "refresh_timeout".into(),
                        checked_at_ms: now_ms(),
                    },
                )
            })
        }))
        .await;
        let mut next = HashMap::new();
        for (id, state) in results {
            next.insert(id, state);
        }
        {
            let mut cache = self.cache.lock().await;
            cache.accounts = next.clone();
            cache.refreshed_at_ms = now_ms();
        }
        for (id, state) in &next {
            if let AccountUsageState::Available { usage } = state {
                self.reconcile_exhaustion(id, usage);
            }
        }
    }

    /// @cc [owner:ghuntley,label:usage;pool] fresh-usage-exhaustion-controls-cooling
    /// A fresh usage sample with an exhausted window and a known future reset MUST move a Healthy
    /// or Cooling account to Cooling until the latest relevant reset. It MUST replace a fallback
    /// cooldown with that deadline and MUST NOT clear cooling from a non-exhausted or failed sample.
    fn reconcile_exhaustion(&self, account_id: &str, usage: &AccountUsage) {
        let current_ms = now_ms();
        let Some(reset_at) = exhausted_reset_ms(usage, current_ms) else {
            return;
        };
        let Some(account) = self.store.get_account(account_id) else {
            return;
        };
        if !matches!(
            account.status,
            AccountStatus::Healthy | AccountStatus::Cooling
        ) {
            return;
        }
        self.store
            .update_account_status(account_id, AccountStatus::Cooling, reset_at);
        self.pool
            .lock()
            .unwrap()
            .set_status(account_id, AccountStatus::Cooling, reset_at);
    }

    async fn fetch_account(&self, account: Account) -> (String, AccountUsageState) {
        let id = account.id.clone();
        if account.backend != BackendId::Codex {
            return (
                id,
                AccountUsageState::Unavailable {
                    reason: "unsupported_backend".into(),
                    checked_at_ms: now_ms(),
                },
            );
        }
        if matches!(
            account.status,
            AccountStatus::Disabled | AccountStatus::AuthError
        ) {
            let reason = if account.status == AccountStatus::Disabled {
                "disabled"
            } else {
                "authentication_failed"
            };
            return (
                id,
                AccountUsageState::Unavailable {
                    reason: reason.into(),
                    checked_at_ms: now_ms(),
                },
            );
        }
        let token = match self.tokens.access_token(&account).await {
            Ok(token) => token,
            Err(_) => {
                return (
                    id,
                    AccountUsageState::Unavailable {
                        reason: "authentication_failed".into(),
                        checked_at_ms: now_ms(),
                    },
                );
            }
        };
        let mut response = self.request_usage(&account, &token).await;
        if response
            .as_ref()
            .is_some_and(|response| response.status() == reqwest::StatusCode::UNAUTHORIZED)
        {
            response = match self.tokens.force_refresh(&account.id).await {
                Ok(token) => self.request_usage(&account, &token).await,
                Err(_) => None,
            };
        }
        let parsed = match response {
            Some(resp) if resp.status().is_success() => {
                resp.json::<Value>().await.ok().and_then(parse_usage)
            }
            _ => None,
        };
        match parsed {
            Some(usage) => (id, AccountUsageState::Available { usage }),
            None => (
                id,
                AccountUsageState::Unavailable {
                    reason: "upstream_unavailable".into(),
                    checked_at_ms: now_ms(),
                },
            ),
        }
    }

    async fn request_usage(&self, account: &Account, token: &str) -> Option<reqwest::Response> {
        let url = std::env::var("UNDERCLASS_USAGE_UPSTREAM")
            .unwrap_or_else(|_| "https://chatgpt.com/backend-api/wham/usage".into());
        let mut request = self
            .client
            .get(url)
            .bearer_auth(token)
            .header("User-Agent", crate::codex::USER_AGENT)
            .timeout(std::time::Duration::from_secs(10));
        if let Some(account_id) = &account.account_id {
            request = request.header("ChatGPT-Account-Id", account_id);
        }
        request.send().await.ok()
    }
}

fn exhausted_reset_ms(usage: &AccountUsage, current_ms: i64) -> Option<i64> {
    let windows: Vec<&UsageWindow> = [
        usage.primary_window.as_ref(),
        usage.secondary_window.as_ref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let explicitly_exhausted: Vec<&UsageWindow> = windows
        .iter()
        .copied()
        .filter(|window| window.used_percent >= 100.0)
        .collect();
    let relevant = if explicitly_exhausted.is_empty() {
        if usage.limit_reached {
            windows
        } else {
            return None;
        }
    } else {
        explicitly_exhausted
    };
    if relevant.is_empty() || relevant.iter().any(|window| window.reset_at <= 0) {
        return None;
    }
    let reset_ms = relevant
        .iter()
        .map(|window| window.reset_at.saturating_mul(1000))
        .max()?;
    (reset_ms > current_ms).then_some(reset_ms)
}

fn parse_window(value: Option<&Value>) -> Option<UsageWindow> {
    let value = value?;
    Some(UsageWindow {
        used_percent: value.get("used_percent")?.as_f64()?.clamp(0.0, 100.0),
        limit_window_seconds: value.get("limit_window_seconds")?.as_i64()?,
        reset_after_seconds: value
            .get("reset_after_seconds")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| {
                value
                    .get("reset_at")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    .saturating_sub(now_ms() / 1000)
            })
            .max(0),
        reset_at: value.get("reset_at")?.as_i64()?,
    })
}

fn parse_usage(value: Value) -> Option<AccountUsage> {
    let details = value.get("rate_limit")?;
    let usage = AccountUsage {
        plan_type: value
            .get("plan_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        allowed: details
            .get("allowed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        limit_reached: details
            .get("limit_reached")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        primary_window: parse_window(details.get("primary_window")),
        secondary_window: parse_window(details.get("secondary_window")),
        fetched_at_ms: now_ms(),
    };
    (usage.primary_window.is_some() || usage.secondary_window.is_some()).then_some(usage)
}

#[derive(Clone, Debug, PartialEq)]
pub struct PooledWindow {
    pub used_percent: i32,
    pub limit_window_seconds: i32,
    pub reset_after_seconds: i32,
    pub reset_at: i32,
    pub reporting_accounts: usize,
}

/// @cc [owner:ghuntley,label:usage] pooled-usage-explicit-coverage
/// Each pooled window MUST be the equal-account arithmetic mean of available percentages for one
/// exact window duration across both upstream window positions. Missing, failed, disabled, and
/// auth-error accounts MUST NOT contribute a zero. The API result MUST report eligible, sampled,
/// and per-window reporting account counts and MUST be marked estimated.
pub fn aggregate_windows(
    eligible: &[&Account],
    states: &HashMap<String, AccountUsageState>,
    now_seconds: i64,
) -> Vec<PooledWindow> {
    let mut by_duration: BTreeMap<i64, Vec<&UsageWindow>> = BTreeMap::new();
    for account in eligible {
        if let Some(AccountUsageState::Available { usage }) = states.get(&account.id) {
            for window in [
                usage.primary_window.as_ref(),
                usage.secondary_window.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                by_duration
                    .entry(window.limit_window_seconds)
                    .or_default()
                    .push(window);
            }
        }
    }
    by_duration
        .into_iter()
        .map(|(_, windows)| {
            let used = windows
                .iter()
                .map(|window| window.used_percent)
                .sum::<f64>()
                / windows.len() as f64;
            let reset_at = windows
                .iter()
                .map(|window| window.reset_at)
                .filter(|reset| *reset > 0)
                .min()
                .unwrap_or(0);
            PooledWindow {
                used_percent: used.ceil().clamp(0.0, 100.0) as i32,
                limit_window_seconds: windows[0].limit_window_seconds.clamp(0, i32::MAX as i64)
                    as i32,
                reset_after_seconds: reset_at
                    .saturating_sub(now_seconds)
                    .clamp(0, i32::MAX as i64) as i32,
                reset_at: reset_at.clamp(0, i32::MAX as i64) as i32,
                reporting_accounts: windows.len(),
            }
        })
        .collect()
}

fn native_window(window: &PooledWindow) -> Value {
    json!({"used_percent": window.used_percent, "limit_window_seconds": window.limit_window_seconds,
        "reset_after_seconds": window.reset_after_seconds, "reset_at": window.reset_at})
}

pub fn pooled_value(
    accounts: &[Account],
    states: &HashMap<String, AccountUsageState>,
    current_ms: i64,
) -> Value {
    let eligible: Vec<&Account> = accounts
        .iter()
        .filter(|a| {
            a.backend == BackendId::Codex
                && matches!(a.status, AccountStatus::Healthy | AccountStatus::Cooling)
        })
        .collect();
    let mut windows = aggregate_windows(&eligible, states, current_ms / 1000);
    windows.sort_by_key(|window| window.limit_window_seconds);
    let primary = windows.first().cloned();
    let secondary = windows.get(1).cloned();
    let available: Vec<&AccountUsage> = eligible
        .iter()
        .filter_map(|account| match states.get(&account.id) {
            Some(AccountUsageState::Available { usage }) => Some(usage),
            _ => None,
        })
        .collect();
    let allowed = available
        .iter()
        .any(|usage| usage.allowed && !usage.limit_reached);
    let oldest_sample_ms = available.iter().map(|usage| usage.fetched_at_ms).min();
    let window_coverage: Vec<Value> = windows
        .iter()
        .map(|window| {
            json!({
                "limit_window_seconds": window.limit_window_seconds,
                "reporting_accounts": window.reporting_accounts,
            })
        })
        .collect();
    let plans: std::collections::BTreeSet<&str> = eligible
        .iter()
        .filter_map(|a| match states.get(&a.id) {
            Some(AccountUsageState::Available { usage }) => Some(usage.plan_type.as_str()),
            _ => None,
        })
        .collect();
    let plan = if plans.len() == 1 {
        plans.iter().next().copied().unwrap_or("unknown")
    } else {
        "unknown"
    };
    let rate_limit = if primary.is_some() || secondary.is_some() {
        Some(json!({
            "allowed": allowed,
            "limit_reached": !allowed,
            "primary_window": primary.as_ref().map(native_window), "secondary_window": secondary.as_ref().map(native_window)
        }))
    } else {
        None
    };
    json!({"plan_type": plan, "rate_limit": rate_limit, "credits": null,
        "_underclass": {"estimated": true, "aggregation": "equal_account_mean",
            "eligible_accounts": eligible.len(), "reporting_accounts": available.len(),
            "coverage_complete": available.len() == eligible.len(), "window_coverage": window_coverage,
            "oldest_sample_ms": oldest_sample_ms,
            "stale": oldest_sample_ms.is_none_or(|sample| current_ms.saturating_sub(sample) > REFRESH_INTERVAL_MS * 2)}})
}

pub async fn pooled(State(state): State<Arc<crate::proxy::AppState>>) -> Response {
    state.usage.refresh().await;
    let states = state.usage.snapshot().await;
    let accounts = state.store.list_accounts();
    Json(pooled_value(&accounts, &states, now_ms())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Account;
    fn account(id: &str, status: AccountStatus) -> Account {
        Account {
            id: id.into(),
            backend: BackendId::Codex,
            label: id.into(),
            refresh_token: None,
            access_token: None,
            expires_at: 0,
            account_id: None,
            residency: None,
            enterprise_url: None,
            status,
            reset_at: 0,
            created_at: 0,
            updated_at: 0,
        }
    }
    fn usage(percent: f64, seconds: i64) -> AccountUsageState {
        AccountUsageState::Available {
            usage: AccountUsage {
                plan_type: "pro".into(),
                allowed: true,
                limit_reached: false,
                primary_window: Some(UsageWindow {
                    used_percent: percent,
                    limit_window_seconds: seconds,
                    reset_after_seconds: 30,
                    reset_at: 2_000_000_000,
                }),
                secondary_window: None,
                fetched_at_ms: 0,
            },
        }
    }
    #[test]
    fn missing_usage_is_not_counted_as_free() {
        let a = account("a", AccountStatus::Healthy);
        let b = account("b", AccountStatus::Healthy);
        let mut states = HashMap::new();
        states.insert(a.id.clone(), usage(80.0, 3600));
        states.insert(
            b.id.clone(),
            AccountUsageState::Unavailable {
                reason: "upstream_unavailable".into(),
                checked_at_ms: 0,
            },
        );
        let p = aggregate_windows(&[&a, &b], &states, 0).remove(0);
        assert_eq!(p.used_percent, 80);
        assert_eq!(p.reporting_accounts, 1);
    }
    #[test]
    fn durations_are_never_mixed() {
        let a = account("a", AccountStatus::Healthy);
        let b = account("b", AccountStatus::Healthy);
        let mut states = HashMap::new();
        states.insert(a.id.clone(), usage(20.0, 18000));
        states.insert(b.id.clone(), usage(90.0, 604800));
        let pooled = aggregate_windows(&[&a, &b], &states, 0);
        assert_eq!(pooled.len(), 2);
        assert!(pooled.iter().all(|window| window.reporting_accounts == 1));
    }
    #[test]
    fn matching_durations_combine_across_window_positions() {
        let a = account("a", AccountStatus::Healthy);
        let b = account("b", AccountStatus::Healthy);
        let mut states = HashMap::new();
        states.insert(a.id.clone(), usage(20.0, 604800));
        let mut second = match usage(60.0, 604800) {
            AccountUsageState::Available { usage } => usage,
            _ => unreachable!(),
        };
        second.secondary_window = second.primary_window.take();
        states.insert(b.id.clone(), AccountUsageState::Available { usage: second });
        let pooled = aggregate_windows(&[&a, &b], &states, 0);
        assert_eq!(pooled.len(), 1);
        assert_eq!(pooled[0].used_percent, 40);
        assert_eq!(pooled[0].reporting_accounts, 2);
    }
    #[test]
    fn parses_native_usage_shape() {
        let parsed=parse_usage(json!({"plan_type":"pro","rate_limit":{"primary_window":{"used_percent":34.5,"limit_window_seconds":18000,"reset_after_seconds":12,"reset_at":2_000_000_000},"secondary_window":null}})).unwrap();
        assert_eq!(parsed.plan_type, "pro");
        assert_eq!(parsed.primary_window.unwrap().used_percent, 34.5);
    }

    #[test]
    fn fresh_exhaustion_replaces_fallback_and_blocks_routing_until_reset() {
        let current_ms = now_ms();
        let reset_ms = current_ms + 60_000;
        let store = Arc::new(Store::in_memory().unwrap());
        let mut stored = account("a", AccountStatus::Cooling);
        stored.reset_at = current_ms + 30 * 60_000;
        store.upsert_account(&stored);
        let pool = Arc::new(SyncMutex::new(crate::pool::PoolCore::new(&store)));
        pool.lock()
            .unwrap()
            .set_catalog(BackendId::Codex, vec!["model".into()]);
        let client = reqwest::Client::new();
        let tokens = Arc::new(TokenManager::new(store.clone(), client.clone()));
        let service = UsageService::new(store.clone(), tokens, client, pool.clone());
        let sample = AccountUsage {
            plan_type: "pro".into(),
            allowed: false,
            limit_reached: true,
            primary_window: Some(UsageWindow {
                used_percent: 100.0,
                limit_window_seconds: 18_000,
                reset_after_seconds: 60,
                reset_at: reset_ms / 1000,
            }),
            secondary_window: None,
            fetched_at_ms: current_ms,
        };
        service.reconcile_exhaustion("a", &sample);
        assert_eq!(
            store.get_account("a").unwrap().reset_at,
            reset_ms / 1000 * 1000
        );
        assert!(matches!(
            pool.lock().unwrap().select(current_ms, None, "model"),
            Err(crate::pool::SelectError::Saturated { .. })
        ));
        assert!(
            pool.lock()
                .unwrap()
                .select(reset_ms / 1000 * 1000, None, "model")
                .is_ok()
        );
    }

    #[test]
    fn unknown_reset_never_changes_health() {
        let store = Arc::new(Store::in_memory().unwrap());
        let stored = account("a", AccountStatus::Healthy);
        store.upsert_account(&stored);
        let pool = Arc::new(SyncMutex::new(crate::pool::PoolCore::new(&store)));
        let client = reqwest::Client::new();
        let tokens = Arc::new(TokenManager::new(store.clone(), client.clone()));
        let service = UsageService::new(store.clone(), tokens, client, pool);
        let sample = AccountUsage {
            plan_type: "pro".into(),
            allowed: false,
            limit_reached: true,
            primary_window: Some(UsageWindow {
                used_percent: 99.0,
                limit_window_seconds: 18_000,
                reset_after_seconds: 0,
                reset_at: 0,
            }),
            secondary_window: None,
            fetched_at_ms: now_ms(),
        };
        service.reconcile_exhaustion("a", &sample);
        assert_eq!(
            store.get_account("a").unwrap().status,
            AccountStatus::Healthy
        );
    }

    #[test]
    fn multiple_exhausted_windows_use_latest_reset() {
        let current_ms = now_ms();
        let usage = AccountUsage {
            plan_type: "pro".into(),
            allowed: false,
            limit_reached: true,
            primary_window: Some(UsageWindow {
                used_percent: 100.0,
                limit_window_seconds: 18_000,
                reset_after_seconds: 60,
                reset_at: current_ms / 1000 + 60,
            }),
            secondary_window: Some(UsageWindow {
                used_percent: 100.0,
                limit_window_seconds: 604_800,
                reset_after_seconds: 120,
                reset_at: current_ms / 1000 + 120,
            }),
            fetched_at_ms: current_ms,
        };
        assert_eq!(
            exhausted_reset_ms(&usage, current_ms),
            Some((current_ms / 1000 + 120) * 1000)
        );
    }
}
