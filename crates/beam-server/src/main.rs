//! `beam`: a web interface and data feed for your datasets.

mod api;
mod app;
mod auth;
mod catalog;
mod chart;
mod config;
mod curate;
mod export;
mod feed;
mod mcp;
mod model;
mod net;
mod people;
mod pgsql;
mod postgres;
mod review;
mod spc;
mod stacks;
mod system;
mod table;
#[cfg(test)]
mod tests;
mod uploads;
mod web;

use beam_accounts::{Role, Store};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "beam",
    version,
    about = "Open Beam: explore your data and serve it to Excel, Power BI and other tools"
)]
struct Cli {
    /// Path to the configuration file. Without one, Open Beam uses beam.toml
    /// in the current folder if there is one, and otherwise starts with
    /// personal settings in an OpenBeam folder in your home folder.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    /// Do not open a browser window on start.
    #[arg(long, global = true)]
    no_browser: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the server (the default).
    Serve,
    /// Create an administrator account. The password is read from BEAM_ADMIN_PASSWORD.
    CreateAdmin { username: String },
}

/// Threads that run blocking work (queries, password checks) at once. Each
/// can hold its own memory pool, so a fixed number keeps memory bounded under
/// load; the engine's own gate already limits how many queries run.
const BLOCKING_THREADS: usize = 32;

fn main() -> ExitCode {
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(BLOCKING_THREADS)
        .build()
    {
        Ok(runtime) => runtime.block_on(run()),
        Err(e) => {
            eprintln!("beam: cannot start: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> ExitCode {
    let cli = Cli::parse();
    let outcome = match load_config(cli.config.as_deref()) {
        Err(message) => Err(message),
        Ok((config, personal)) => match cli.command {
            Some(Command::CreateAdmin { username }) => create_admin(&config, &username),
            Some(Command::Serve) | None => serve(config, personal && !cli.no_browser).await,
        },
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("beam: {message}");
            ExitCode::FAILURE
        }
    }
}

fn open_accounts(config: &config::Config) -> Result<Store, String> {
    Store::open(&config.accounts).map_err(|e| format!("cannot open {}: {e}", config.accounts.display()))
}

/// The configuration to use, and whether it is the personal default.
fn load_config(path: Option<&std::path::Path>) -> Result<(config::Config, bool), String> {
    if let Some(path) = path {
        return Ok((config::Config::load(path)?, false));
    }
    let local = std::path::Path::new("beam.toml");
    if local.exists() {
        return Ok((config::Config::load(local)?, false));
    }
    let dir = config::personal_dir().ok_or("cannot find your home folder; pass --config")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok((config::Config::personal(&dir), true))
}

/// Opens the address in the default browser. Failure is harmless: the
/// address is also printed.
fn open_browser(url: &str) {
    let mut command = if cfg!(target_os = "windows") {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else {
        std::process::Command::new("xdg-open")
    };
    let _ = command
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn create_admin(config: &config::Config, username: &str) -> Result<(), String> {
    let password = std::env::var(config::ADMIN_PASSWORD_ENV)
        .map_err(|_| format!("set {} to the new administrator's password", config::ADMIN_PASSWORD_ENV))?;
    let account = open_accounts(config)?
        .create(username, &password, Role::Admin)
        .map_err(|e| e.to_string())?;
    println!("Created administrator '{}'.", account.username);
    Ok(())
}

/// Re-adds the datasets people uploaded before the last restart. One that
/// cannot be read is reported and skipped; the rest still start.
fn restore_uploads(engine: &beam_engine::Engine, accounts: &Store) {
    if engine.upload_dir().is_none() {
        return;
    }
    for upload in accounts.uploads().unwrap_or_default() {
        let cleaning = beam_engine::Cleaning {
            drop_empty_rows: upload.cleaning.drop_empty_rows,
            drop_duplicate_rows: upload.cleaning.drop_duplicate_rows,
        };
        let format = uploads::format_of(&upload.file);
        if let Err(e) = engine.add_upload_cleaned(&upload.name, &upload.file, format, cleaning) {
            eprintln!("beam: uploaded dataset '{}' is unavailable: {e}", upload.name);
        }
    }
}

/// Refuses to start when a configured or joined dataset has the name of a
/// saved upload. Descriptions, verified questions and watches are kept by
/// dataset name, so the new dataset, which everyone can see, would show what
/// the upload's owner wrote about their private data.
fn refuse_names_taken_by_uploads(engine: &beam_engine::Engine, accounts: &Store) -> Result<(), String> {
    let ours: std::collections::HashSet<String> =
        engine.datasets().iter().map(|d| d.name.to_ascii_lowercase()).collect();
    let clashes: Vec<String> = accounts
        .uploads()
        .unwrap_or_default()
        .iter()
        .filter(|u| ours.contains(&u.name.to_ascii_lowercase()))
        .map(|u| {
            let owner = accounts
                .get(u.owner_id)
                .map_or("an account".to_string(), |a| a.username);
            format!("{} (uploaded by {owner})", u.name)
        })
        .collect();
    if clashes.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "these names belong to uploads, so a configured or joined dataset cannot use them: {}. \
             Rename the dataset in beam.toml, or give the link its own `joined` name.",
            clashes.join(", ")
        ))
    }
}

/// The model named by `ontology`, checked against the configured datasets.
/// A model with problems stops the server, listing all of them, as any other
/// mistake in the configuration does.
fn load_model(config: &config::Config, engine: &beam_engine::Engine) -> Result<Option<model::Model>, String> {
    let Some(path) = &config.ontology else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    model::parse(&text, &model::configured_sets(engine))
        .map(Some)
        .map_err(|problems| format!("{}:\n  {}", path.display(), problems.join("\n  ")))
}

async fn serve(config: config::Config, open: bool) -> Result<(), String> {
    let accounts = open_accounts(&config)?;
    let account_count = accounts.count().map_err(|e| e.to_string())?;
    config::check_exposure(&config, account_count)?;
    let upload_dir = (config.max_upload_mb > 0).then_some(config.uploads.as_path());
    let engine = beam_engine::Engine::open_with_options(&config.dataset_specs(), upload_dir, &config.limits())
        .map_err(|e| e.to_string())?;
    engine.set_query_timeout(std::time::Duration::from_secs(config.query_timeout_seconds));
    // Checked, and its joined datasets made, before anything listens and
    // before uploads return, so a mistake stops the start and configured
    // names always win.
    let model = load_model(&config, &engine)?;
    if let Some(model) = &model {
        model.add_joins(&engine)?;
    }
    refuse_names_taken_by_uploads(&engine, &accounts)?;
    restore_uploads(&engine, &accounts);
    // Copies left by datasets that no longer exist, or whose files changed.
    engine.prune_cache();
    review::remove_expired_drafts(&engine, &accounts, review::DRAFT_DAYS);

    let postgres = match config.postgres_bind {
        Some(bind) => Some(
            tokio::net::TcpListener::bind(bind)
                .await
                .map_err(|e| format!("cannot listen for PostgreSQL clients on {bind}: {e}"))?,
        ),
        None => None,
    };
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", config.bind))?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let base = config.public_url.clone().unwrap_or_else(|| format!("http://{address}"));
    println!(
        "Open Beam {} serving {} dataset(s)",
        env!("CARGO_PKG_VERSION"),
        engine.datasets().len()
    );
    println!("  Open in your browser:        {base}/");
    println!("  Data feed (Excel, Power BI): {base}/odata/");
    if let Some(bind) = postgres.as_ref().and_then(|l| l.local_addr().ok()) {
        println!("  PostgreSQL clients (read-only): {bind}  (password: a personal API token)");
        if !bind.ip().is_loopback() {
            println!("  Warning: PostgreSQL access is unencrypted. Put it behind a TLS tunnel before using it across a network.");
        }
    }
    if account_count == 0 {
        println!("  No accounts yet. Open the address above to create the administrator.");
    }
    println!("  Keep this window open while you use Open Beam. Close it to stop.");
    if open {
        open_browser(&format!("{base}/"));
    }

    let mut state = app::AppState::new(
        engine,
        accounts,
        config.max_page_size,
        config.public_url,
        address.to_string(),
    );
    state.max_upload_bytes = config.max_upload_mb.saturating_mul(1024 * 1024);
    state.model = model.map(Arc::new);
    state.connections = Arc::new(net::Connections::with_trusted_proxies(config.trusted_proxies.clone()));
    let state = Arc::new(state);
    // Unreviewed drafts are swept hourly, not only at start-up.
    let sweeper = state.clone();
    tokio::spawn(async move {
        let mut hourly = tokio::time::interval(std::time::Duration::from_secs(3600));
        // A slow run must not make the next ones fire back to back.
        hourly.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            hourly.tick().await;
            let worker = sweeper.clone();
            let _ = tokio::task::spawn_blocking(move || {
                review::remove_expired_drafts(&worker.engine, &worker.accounts, review::DRAFT_DAYS)
            })
            .await;
            // Watches run at start-up and then every hour.
            curate::run_all(&sweeper).await;
        }
    });
    if let Some(postgres) = postgres {
        tokio::spawn(postgres::serve(postgres, state.clone()));
    }
    let connections = state.connections.clone();
    net::serve(listener, app::router(state), connections, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .map_err(|e| e.to_string())
}
