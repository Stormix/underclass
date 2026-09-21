use underclass::provider::BackendMap;
use underclass::{cli, codex, config, copilot, flows, logging, models, pool, proxy, store, tokens, ui, usage};

use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(name = "underclass", about = "pooled multi-subscription codex/copilot proxy for opencode")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, value_enum, default_value = "auto", global = true)]
    log_format: logging::LogFormat,
}

#[derive(Subcommand)]
enum Command {
    /// run the proxy server
    Serve {
        #[arg(long)]
        bind: Option<String>,
    },
    /// configure opencode to use this pool
    Connect {
        #[clap(flatten)]
        args: cli::ConnectArgs,
    },
}

fn main() {
    let cli = Cli::parse();
    logging::init(cli.log_format);
    match cli.command {
        Some(Command::Connect { mut args }) => {
            let cfg = config::Config::load();
            let store = store::Store::open(&cfg.db_path()).expect("open pool db");
            prefer_configured_proxy_key(&mut args, cfg.proxy_key);
            if let Err(e) = cli::connect(&args, &store) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Some(Command::Serve { bind }) => serve(bind).expect("server failed"),
        None => serve(None).expect("server failed"),
    }
}

/// @cc [owner:Stormix,label:cli;config] connect-active-proxy-key
/// When `connect` receives no explicit API key, it MUST use a non-empty configured proxy key
/// before the legacy key stored in the database. An explicit API key MUST remain unchanged.
fn prefer_configured_proxy_key(args: &mut cli::ConnectArgs, configured_key: Option<String>) {
    if args.api_key.is_none() {
        args.api_key = configured_key.filter(|key| !key.is_empty());
    }
}

fn serve(bind_override: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_serve(bind_override))
}

fn is_loopback_bind(bind: &str) -> bool {
    bind
        .strip_prefix("127.0.0.1:")
        .is_some_and(|port| !port.is_empty())
        || bind
            .strip_prefix("[::1]:")
            .is_some_and(|port| !port.is_empty())
}

/// @cc [owner:ghuntley,label:security] keys-minted-once
/// The proxy API key and a non-empty admin UI token MUST be minted on first run, persisted to the
/// store, and reused on every subsequent start; explicitly configured values MUST take precedence.
/// An explicitly empty UI token MUST disable UI authentication only on a loopback bind. Key and
/// token values MUST NOT be written to diagnostics.
async fn async_serve(bind_override: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = config::Config::load();
    let bind = bind_override.unwrap_or_else(|| cfg.bind.clone());
    let store = Arc::new(store::Store::open(&cfg.db_path())?);
    store.prune_bindings(
        models::now_ms(),
        pool::BINDING_TTL_MS,
        pool::DEFAULT_BINDING_CAP,
    );

    let proxy_key = match cfg.proxy_key.clone() {
        Some(key) if !key.is_empty() => Some(key),
        _ => match store.config_get("proxy_key") {
            Some(key) => Some(key),
            None => {
                let key = format!("sk-underclass-{}", crate::models::new_id().replace('-', ""));
                store.config_set("proxy_key", &key);
                Some(key)
            }
        },
    };

    let ui_token = match cfg.ui_token.clone() {
        Some(token) if token.is_empty() && is_loopback_bind(&bind) => None,
        Some(token) if token.is_empty() => {
            return Err("ui_token can be disabled only on a loopback bind".into());
        }
        Some(token) => Some(token),
        None => match store.config_get("ui_token") {
            Some(token) => Some(token),
            None => {
                let token = crate::models::new_id().replace('-', "");
                store.config_set("ui_token", &token);
                Some(token)
            }
        },
    };

    seed_catalog(&store, models::BackendId::Codex, codex::default_catalog());
    seed_catalog(&store, models::BackendId::Copilot, copilot::fallback_catalog());

    let core = pool::PoolCore::new(&store);
    let pool = Arc::new(Mutex::new(core));

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()?;

    let tokens = Arc::new(tokens::TokenManager::new(store.clone(), client.clone()));

    let mut backends: BackendMap = HashMap::new();
    backends.insert(
        models::BackendId::Codex,
        Arc::new(codex::CodexBackend {
            cooldown_ms: cfg.codex_cooldown_ms,
        }),
    );
    backends.insert(
        models::BackendId::Copilot,
        Arc::new(copilot::CopilotBackend {
            cooldown_ms: cfg.copilot_cooldown_ms,
        }),
    );
    let backends = Arc::new(backends);

    let usage = Arc::new(usage::UsageService::new(
        store.clone(),
        tokens.clone(),
        client.clone(),
        pool.clone(),
    ));
    let state = Arc::new(proxy::AppState {
        store: store.clone(),
        pool: pool.clone(),
        tokens: tokens.clone(),
        backends: backends.clone(),
        client: client.clone(),
        logs: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        flows: flows::FlowRegistry::default(),
        proxy_key,
        ui_token: ui_token.clone(),
        usage: usage.clone(),
    });

    {
        let store = store.clone();
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                store.prune_bindings(
                    models::now_ms(),
                    pool::BINDING_TTL_MS,
                    pool::DEFAULT_BINDING_CAP,
                );
                state.pool.lock().unwrap().sync_from_store(&store);
            }
        });
    }

    {
        let usage = usage.clone();
        tokio::spawn(async move {
            loop {
                usage.refresh().await;
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
    }

    refresh_copilot_catalogs_on_boot(&state).await;
    refresh_identities_on_boot(&state).await;

    let v1 = axum::Router::new()
        .route("/models", axum::routing::get(proxy::models))
        .route("/responses", axum::routing::post(proxy::infer))
        .route("/chat/completions", axum::routing::post(proxy::infer))
        .route("/usage", axum::routing::get(usage::pooled))
        .route("/api/codex/usage", axum::routing::get(usage::pooled))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            proxy::require_proxy_key,
        ))
        .with_state(state.clone());

    let native_usage = axum::Router::new()
        .route("/api/codex/usage", axum::routing::get(usage::pooled))
        .route("/backend-api/wham/usage", axum::routing::get(usage::pooled))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            proxy::require_proxy_key,
        ))
        .with_state(state.clone());

    let app = axum::Router::new()
        .route("/admin/api/state", axum::routing::get(ui::state))
        .route("/admin/api/flows", axum::routing::post(ui::start_flow))
        .route("/admin/api/flows/{id}", axum::routing::get(ui::flow_status))
        .route(
            "/admin/api/accounts/{id}",
            axum::routing::delete(ui::delete_account),
        )
        .route(
            "/admin/api/accounts/{id}/disable",
            axum::routing::post(ui::disable_account),
        )
        .route(
            "/admin/api/accounts/{id}/enable",
            axum::routing::post(ui::enable_account),
        )
        .route(
            "/admin/api/accounts/{id}/relogin",
            axum::routing::post(ui::relogin_account),
        )
        .route(
            "/admin/api/catalog/{backend}",
            axum::routing::get(ui::get_catalog).put(ui::put_catalog),
        )
        .route(
            "/admin/api/catalog/refresh-copilot",
            axum::routing::post(ui::refresh_copilot_catalog),
        )
        .route("/admin/api/client-key", axum::routing::get(ui::client_key))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            ui::require_ui_token,
        ))
        .merge(native_usage)
        .route("/", axum::routing::get(ui::index))
        .nest("/v1", v1)
        .layer(axum::middleware::from_fn(underclass::correlation::middleware))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("underclass listening on http://{bind}");
    println!("web ui: http://{bind}/");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_loopback_bind, prefer_configured_proxy_key};
    use underclass::cli::ConnectArgs;

    #[test]
    fn recognizes_only_ip_loopback_binds() {
        assert!(is_loopback_bind("127.0.0.1:80"));
        assert!(is_loopback_bind("[::1]:8080"));
        assert!(!is_loopback_bind("0.0.0.0:80"));
        assert!(!is_loopback_bind("localhost:80"));
    }

    fn connect_args(api_key: Option<&str>) -> ConnectArgs {
        ConnectArgs {
            base_url: "http://127.0.0.1:8080".to_string(),
            api_key: api_key.map(str::to_string),
            model: "gpt-5.5".to_string(),
            no_default_model: false,
            project: false,
            dry_run: false,
            remove: false,
        }
    }

    #[test]
    fn connect_prefers_the_active_configured_proxy_key() {
        let mut from_config = connect_args(None);
        prefer_configured_proxy_key(&mut from_config, Some("configured".to_string()));
        assert_eq!(from_config.api_key.as_deref(), Some("configured"));

        let mut explicit = connect_args(Some("explicit"));
        prefer_configured_proxy_key(&mut explicit, Some("configured".to_string()));
        assert_eq!(explicit.api_key.as_deref(), Some("explicit"));
    }
}

fn seed_catalog(store: &store::Store, backend: models::BackendId, defaults: Vec<models::ModelInfo>) {
    if store.catalog(backend).is_empty() {
        store.set_catalog(backend, &defaults);
    }
}

async fn refresh_copilot_catalogs_on_boot(state: &Arc<proxy::AppState>) {
    let accounts: Vec<models::Account> = state
        .store
        .list_accounts()
        .into_iter()
        .filter(|a| a.backend == models::BackendId::Copilot)
        .collect();
    for account in accounts {
        let Some(token) = account.refresh_token.clone() else {
            continue;
        };
        if let Ok(catalog) = copilot::fetch_catalog(&state.client, &account, &token).await {
            if !catalog.is_empty() {
                state.store.set_catalog(models::BackendId::Copilot, &catalog);
                let ids: Vec<String> = catalog.iter().map(|m| m.id.clone()).collect();
                state.pool.lock().unwrap().set_catalog(models::BackendId::Copilot, ids);
            }
        }
    }
}

fn is_generic_label(label: &str) -> bool {
    matches!(label, "chatgpt" | "github") || label.trim().is_empty()
}

async fn refresh_identities_on_boot(state: &Arc<proxy::AppState>) {
    let accounts: Vec<models::Account> = state
        .store
        .list_accounts()
        .into_iter()
        .filter(|a| is_generic_label(&a.label))
        .collect();
    for account in accounts {
        let identity = match account.backend {
            models::BackendId::Codex => {
                let token = match state.tokens.access_token(&account).await {
                    Ok(token) => token,
                    Err(_) => continue,
                };
                codex::identity_from_token(&token)
            }
            models::BackendId::Copilot => {
                let Some(token) = account.refresh_token.clone() else {
                    continue;
                };
                copilot::fetch_github_identity(&state.client, &token).await
            }
        };
        let Some(identity) = identity else {
            continue;
        };
        let mut updated = account.clone();
        updated.label = identity;
        updated.updated_at = models::now_ms();
        state.store.upsert_account(&updated);
        state.pool.lock().unwrap().sync_account(updated);
        tracing::info!(account = %account.id, "account.identity_refreshed");
    }
}
