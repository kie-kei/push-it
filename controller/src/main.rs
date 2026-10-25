use pushit_controller::auth::Auth;
use pushit_controller::config::Config;
use pushit_controller::db::Db;
use pushit_controller::logs::LogStore;
use pushit_controller::runtime::Runtime;
use pushit_controller::secrets::Secrets;
use pushit_controller::server::{router, AppState};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = Config::from_env()?;
    std::fs::create_dir_all(cfg.apps_dir())?;
    std::fs::create_dir_all(cfg.logs_dir())?;

    let db = Db::open(&cfg.db_path())?;
    let secrets = Secrets::load(&cfg.age_key_path())?;
    let runtime = Runtime::new(Duration::from_secs(cfg.handler_timeout_secs))?;
    let logs = Arc::new(LogStore::new(cfg.logs_dir())?);
    let auth = Auth::try_new(cfg.clone())?;

    let state = Arc::new(AppState {
        cfg: cfg.clone(),
        db,
        secrets,
        runtime,
        logs,
        auth,
        metrics: Default::default(),
    });

    tracing::info!("pushit-controller listening on {}", cfg.bind);
    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
