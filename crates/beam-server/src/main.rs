//! `beam`: a web interface and data feed for your datasets.

mod app;
mod auth;
mod catalog;
mod chart;
mod config;
mod export;
mod feed;
mod people;
mod review;
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

#[tokio::main]
async fn main() -> ExitCode {
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

async fn serve(config: config::Config, open: bool) -> Result<(), String> {
    let accounts = open_accounts(&config)?;
    let account_count = accounts.count().map_err(|e| e.to_string())?;
    config::check_exposure(&config, account_count)?;
    let upload_dir = (config.max_upload_mb > 0).then_some(config.uploads.as_path());
    let engine =
        beam_engine::Engine::open_with_uploads(&config.dataset_specs(), upload_dir).map_err(|e| e.to_string())?;
    restore_uploads(&engine, &accounts);
    review::remove_expired_drafts(&engine, &accounts, review::DRAFT_DAYS);

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
    let state = Arc::new(state);
    // Unreviewed drafts are swept hourly, not only at start-up.
    let sweeper = state.clone();
    tokio::spawn(async move {
        let mut hourly = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            hourly.tick().await;
            let worker = sweeper.clone();
            let _ = tokio::task::spawn_blocking(move || {
                review::remove_expired_drafts(&worker.engine, &worker.accounts, review::DRAFT_DAYS)
            })
            .await;
        }
    });
    axum::serve(listener, app::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| e.to_string())
}
