// TucanoTime — timesheet server: one binary serves the REST API, the web
// GUI, the machine-readable contract (openapi.json) and its docs UI.
// Data lives in one folder tree below TUCANO_DATA_DIR (no database).

use std::sync::Arc;

use tucano_time::api::AppState;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let data_dir = std::env::var("TUCANO_DATA_DIR").unwrap_or_else(|_| "data".to_owned());
    let port = std::env::var("TUCANO_PORT").unwrap_or_else(|_| "8080".to_owned());
    let store = tucano_time::store::Store::open(&data_dir)?;

    // Session key: a configured secret, or an ephemeral per-process key (dev).
    // In production a strong secret is required — refuse to start otherwise (#44).
    let production = std::env::var("TUCANO_ENV").as_deref() == Ok("production");
    let session = tucano_time::auth::make_session(
        std::env::var("TUCANO_SESSION_SECRET")
            .ok()
            .filter(|s| !s.is_empty()),
        production,
    )?;
    let state = AppState::with_session(store, Arc::new(session));

    // Background scheduler (#61) with the reminder job (#22). Recurring
    // invoices (#26) and budget alerts (#30) register here too.
    let jobs: Vec<Arc<dyn tucano_time::scheduler::Job>> = vec![
        Arc::new(tucano_time::reminders::ReminderJob::new(
            state.store.clone(),
        )),
        Arc::new(tucano_time::recurring::RecurringJob::new(
            state.store.clone(),
        )),
        Arc::new(tucano_time::budgets::BudgetAlertJob::new(
            state.store.clone(),
        )),
        Arc::new(tucano_time::email_reminders::EmailReminderJob::new(
            state.store.clone(),
            state.email.clone(),
            std::env::var("TUCANO_REMINDER_DAYS")
                .ok()
                .and_then(|d| d.parse().ok())
                .unwrap_or(7),
            std::path::Path::new(&data_dir),
        )),
    ];
    let scheduler = Arc::new(tucano_time::scheduler::Scheduler::new(
        std::path::Path::new(&data_dir),
        jobs,
    ));
    scheduler.spawn(60);

    let app = tucano_time::build_router(state);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port.parse()?)).await?;
    tracing::info!("tucano-time listening on :{port}, data dir {data_dir}");
    axum::serve(listener, app).await?;
    Ok(())
}
