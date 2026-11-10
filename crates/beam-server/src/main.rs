//! `beam`: a web interface and data feed for your datasets.

mod app;
mod auth;
mod config;
mod feed;
#[cfg(test)]
mod tests;
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
    /// Path to the configuration file.
    #[arg(short, long, default_value = "beam.toml", global = true)]
    config: PathBuf,
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
    let outcome = match cli.command {
        Some(Command::CreateAdmin { username }) => create_admin(&cli.config, &username),
        Some(Command::Serve) | None => serve(&cli.config).await,
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

fn create_admin(config_path: &std::path::Path, username: &str) -> Result<(), String> {
    let config = config::Config::load(config_path)?;
    let password = std::env::var(config::ADMIN_PASSWORD_ENV)
        .map_err(|_| format!("set {} to the new administrator's password", config::ADMIN_PASSWORD_ENV))?;
    let account = open_accounts(&config)?
        .create(username, &password, Role::Admin)
        .map_err(|e| e.to_string())?;
    println!("Created administrator '{}'.", account.username);
    Ok(())
}

async fn serve(config_path: &std::path::Path) -> Result<(), String> {
    let config = config::Config::load(config_path)?;
    let accounts = open_accounts(&config)?;
    let account_count = accounts.count().map_err(|e| e.to_string())?;
    config::check_exposure(&config, account_count)?;
    let engine = beam_engine::Engine::open(&config.dataset_specs()).map_err(|e| e.to_string())?;

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

    let state = app::AppState::new(
        engine,
        accounts,
        config.max_page_size,
        config.public_url,
        address.to_string(),
    );
    axum::serve(listener, app::router(Arc::new(state)))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| e.to_string())
}
