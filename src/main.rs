// TucanoTime — timesheet server: one binary serves the REST API, the web
// GUI, the machine-readable contract (openapi.json) and its docs UI.
// Data lives in one folder tree below TUCANO_DATA_DIR (no database).
//
// The same binary doubles as the operator CLI (#94):
//   tucano-time                 serve (default)
//   tucano-time --backup FILE   archive the data dir to FILE (tar.gz + manifest)
//   tucano-time --restore FILE  unpack a backup into the data dir (--force to overwrite)
//   tucano-time --health        probe the local server's /healthz (container HEALTHCHECK)
//   tucano-time --version       print the version

use std::sync::Arc;

use tucano_time::api::AppState;

fn data_dir() -> std::path::PathBuf {
    std::env::var("TUCANO_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("data"))
}

/// Run one of the operator subcommands. `Ok(true)` means "a CLI action ran,
/// do not serve".
fn run_cli(args: &[String]) -> Result<bool, String> {
    let root = data_dir();
    match args.get(1).map(String::as_str) {
        Some("--version") => {
            println!("tucano-time {}", env!("CARGO_PKG_VERSION"));
            Ok(true)
        }
        Some("--backup") => {
            let Some(out) = args.get(2) else {
                return Err("usage: tucano-time --backup <out.tar.gz>".into());
            };
            let n = tucano_time::backup::create(&root, std::path::Path::new(out))
                .map_err(|e| e.to_string())?;
            println!("backed up {n} file(s) from {} to {out}", root.display());
            Ok(true)
        }
        Some("--restore") => {
            let Some(in_path) = args.get(2) else {
                return Err("usage: tucano-time --restore <in.tar.gz> [--force]".into());
            };
            let force = args.iter().any(|a| a == "--force");
            let notes = tucano_time::backup::restore(std::path::Path::new(in_path), &root, force)
                .map_err(|e| e.to_string())?;
            println!("restored into {}", root.display());
            for n in notes {
                println!("  note: {n}");
            }
            Ok(true)
        }
        Some("--health") => {
            let port = std::env::var("TUCANO_PORT").unwrap_or_else(|_| "8080".into());
            let url = format!("http://127.0.0.1:{port}/healthz");
            let ok = ureq::get(&url)
                .call()
                .map(|r| r.status() == 200u16)
                .unwrap_or(false);
            if ok {
                std::process::exit(0)
            } else {
                std::process::exit(1)
            }
        }
        _ => Ok(false),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if run_cli(&args)? {
        return Ok(());
    }

    let root = data_dir();
    let root_str = root.display().to_string();
    let port = std::env::var("TUCANO_PORT").unwrap_or_else(|_| "8080".to_owned());

    // Open the store first: it creates the data dir that session.key and
    // config.json persistence need.
    let mut store = tucano_time::store::Store::open(&root)?;

    // Externalised configuration (#94): env > <data>/config.json > default.
    // A corrupt config file refuses to start rather than silently reverting
    // settings — that is exactly the failure mode this ticket removes.
    let cfg = Arc::new(
        tucano_time::appconfig::AppConfig::load(store.root()).map_err(|e| {
            tracing::error!("{e}");
            e
        })?,
    );

    // Session key: env secret > env *FILE > <data>/session.key (auto-created
    // with fresh randomness on first boot). A restart/update of the container
    // therefore keeps everybody signed in without any operator setup (#94).
    let production = std::env::var("TUCANO_ENV").as_deref() == Ok("production");
    let (secret, _) = tucano_time::auth::resolve_session_secret(
        store.root(),
        std::env::var("TUCANO_SESSION_SECRET").ok(),
        std::env::var("TUCANO_SESSION_SECRET_FILE").ok(),
        production,
    );
    let session = tucano_time::auth::make_session(secret, production)?;

    // Vault: hard-fail when the store exists but the key cannot open it
    // (#94 — never start with a silently disabled Settings tab).
    let vault = tucano_time::vault::open_for_boot(
        store.root(),
        std::env::var("TUCANO_SECRET_KEY").ok(),
        std::env::var("TUCANO_SECRET_KEY_FILE").ok(),
    )
    .map_err(|e| {
        tracing::error!("{e}");
        e
    })?;

    // Respect a config-file max_docs override (env is handled inside Store::open).
    if std::env::var("TUCANO_MAX_DOCS").is_err() {
        let max = cfg.get_int("max_docs", &tucano_time::appconfig::process_env);
        if max > 0 {
            store = store.with_max_docs(max as usize);
        }
    }
    let state = AppState::boot(store, Arc::new(session), vault, cfg.clone());

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
            cfg.get_int("reminder_days", &tucano_time::appconfig::process_env),
            &root,
        )),
        Arc::new(tucano_time::accounting::AccountingRetryJob::new(
            state.store.clone(),
            state.accounting.clone(),
        )),
    ];
    let scheduler = Arc::new(tucano_time::scheduler::Scheduler::new(&root, jobs));
    scheduler.spawn(60);

    let app = tucano_time::build_router(state);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port.parse()?)).await?;
    tracing::info!("tucano-time listening on :{port}, data dir {root_str}");
    axum::serve(listener, app).await?;
    Ok(())
}
