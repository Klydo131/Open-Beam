//! `beam`: serves local datasets to Excel, Power BI and other OData clients.

mod app;
mod config;

use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "beam",
    version,
    about = "Open Beam: serve your data to Excel, Power BI and other analytics tools"
)]
struct Cli {
    /// Path to the configuration file.
    #[arg(short, long, default_value = "beam.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    match serve(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("beam: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(cli: Cli) -> Result<(), String> {
    let config = config::Config::load(&cli.config)?;
    let credentials = config::credentials(&config, std::env::var(config::PASSWORD_ENV).ok())?;
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
    println!("  OData feed: {base}/odata/");
    println!(
        "  Sign-in:    {}",
        if credentials.is_some() {
            "Basic (username and password)"
        } else {
            "none (local only)"
        }
    );

    let state = app::AppState::new(
        engine,
        config.max_page_size,
        config.public_url,
        address.to_string(),
        credentials,
    );
    axum::serve(listener, app::router(Arc::new(state)))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| e.to_string())
}
