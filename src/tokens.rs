use crate::jwt;
use crate::models::{now_ms, Account};
use crate::store::Store;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;

const PROACTIVE_REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("account has no stored credential")]
    MissingToken,
    #[error("token refresh failed: {0}")]
    RefreshFailed(String),
}

pub struct TokenManager {
    store: Arc<Store>,
    client: Client,
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl TokenManager {
    pub fn new(store: Arc<Store>, client: Client) -> Self {
        Self {
            store,
            client,
            locks: Mutex::new(HashMap::new()),
        }
    }

    fn lock_for(&self, account_id: &str) -> Arc<AsyncMutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(account_id.to_string())
            .or_default()
            .clone()
    }

    pub async fn access_token(&self, account: &Account) -> Result<String, TokenError> {
        match account.backend {
            crate::models::BackendId::Copilot => account
                .refresh_token
                .clone()
                .ok_or(TokenError::MissingToken),
            crate::models::BackendId::Codex => {
                if let Some(token) = &account.access_token {
                    if account.expires_at.saturating_sub(PROACTIVE_REFRESH_MARGIN_MS) > now_ms() {
                        return Ok(token.clone());
                    }
                }
                self.force_refresh(&account.id).await
            }
        }
    }

    pub async fn force_refresh(&self, account_id: &str) -> Result<String, TokenError> {
        let lock = self.lock_for(account_id).clone();
        let _guard = lock.lock().await;

        let account = self
            .store
            .get_account(account_id)
            .ok_or(TokenError::MissingToken)?;

        let refresh_token = account.refresh_token.clone().ok_or(TokenError::MissingToken)?;

        match account.backend {
            crate::models::BackendId::Copilot => Ok(refresh_token),
            crate::models::BackendId::Codex => {
                let tokens = crate::codex::refresh(&self.client, &refresh_token)
                    .await
                    .map_err(|e| TokenError::RefreshFailed(e.to_string()))?;
                let claims = jwt::parse_jwt_claims(&tokens.access_token);
                let chatgpt_account_id = claims
                    .as_ref()
                    .and_then(|c| jwt::extract_account_id(c))
                    .or(account.account_id.clone());
                let residency = claims.as_ref().and_then(|c| jwt::extract_residency(c));
                let expires_at = now_ms() + (tokens.expires_in.unwrap_or(3600) as i64) * 1000;
                self.store.update_tokens(
                    account_id,
                    Some(&tokens.refresh_token),
                    Some(&tokens.access_token),
                    expires_at,
                    chatgpt_account_id.as_deref(),
                    residency.as_deref(),
                );
                Ok(tokens.access_token)
            }
        }
    }
}
