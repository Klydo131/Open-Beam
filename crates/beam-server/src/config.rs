//! `beam.toml` loading and validation.

use beam_engine::{DatasetSpec, Format, Source};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Environment variable holding the password for `beam create-admin`.
pub const ADMIN_PASSWORD_ENV: &str = "BEAM_ADMIN_PASSWORD";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    /// External base URL when served behind a reverse proxy, for example
    /// `https://beam.example.org`. Used to build links in responses.
    pub public_url: Option<String>,
    #[serde(default = "default_page_size")]
    pub max_page_size: u64,
    /// SQLite file that holds accounts and sessions. Created on first run.
    #[serde(default = "default_accounts")]
    pub accounts: PathBuf,
    /// Folder for data people add in the browser. Created on first run.
    #[serde(default = "default_uploads")]
    pub uploads: PathBuf,
    /// Largest file accepted on the Add data page, in megabytes. 0 turns
    /// adding data off.
    #[serde(default = "default_max_upload_mb")]
    pub max_upload_mb: usize,
    /// Longest a single query may run before it is cancelled, in seconds.
    #[serde(default = "default_query_timeout_seconds")]
    pub query_timeout_seconds: u64,
    #[serde(default, rename = "dataset")]
    pub datasets: Vec<DatasetConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetConfig {
    pub name: String,
    /// A data file. Exactly one of `path` and `sql` must be set.
    pub path: Option<PathBuf>,
    pub format: Option<FormatConfig>,
    /// A `SELECT` statement over datasets declared earlier in the file.
    pub sql: Option<String>,
    pub key: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FormatConfig {
    Csv,
    Parquet,
}

fn default_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8787))
}

fn default_accounts() -> PathBuf {
    PathBuf::from("beam-accounts.sqlite")
}

fn default_uploads() -> PathBuf {
    PathBuf::from("beam-uploads")
}

fn default_query_timeout_seconds() -> u64 {
    beam_engine::DEFAULT_QUERY_TIMEOUT.as_secs()
}

fn default_max_upload_mb() -> usize {
    crate::app::DEFAULT_MAX_UPLOAD_MB
}

fn default_page_size() -> u64 {
    5000
}

/// Where Open Beam keeps its files when started without a configuration
/// file: a folder named `OpenBeam` in the person's home folder.
pub fn personal_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join("OpenBeam"))
}

impl Config {
    /// The settings used when Open Beam is simply double-clicked: this
    /// computer only, no built-in datasets, accounts and uploads in `dir`.
    pub fn personal(dir: &Path) -> Self {
        let mut config: Config = toml::from_str("").unwrap_or_else(|_| unreachable!("every setting has a default"));
        config.accounts = dir.join("accounts.sqlite");
        config.uploads = dir.join("uploads");
        config
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let mut config: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if config.max_page_size == 0 {
            return Err("max_page_size must be at least 1".into());
        }
        if let Some(url) = &mut config.public_url {
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err("public_url must start with http:// or https://".into());
            }
            while url.ends_with('/') {
                url.pop();
            }
        }
        // Dataset paths are relative to the config file, not the working directory.
        let base = path.parent().unwrap_or(Path::new("."));
        config.accounts = base.join(&config.accounts);
        config.uploads = base.join(&config.uploads);
        if !(1..=3600).contains(&config.query_timeout_seconds) {
            return Err("query_timeout_seconds must be between 1 and 3600".into());
        }
        if config.max_upload_mb > 4096 {
            return Err("max_upload_mb must be at most 4096".into());
        }
        for dataset in &mut config.datasets {
            match (&mut dataset.path, &dataset.sql, dataset.format) {
                (Some(path), None, _) => *path = base.join(&*path),
                (None, Some(_), None) => {}
                (None, Some(_), Some(_)) => {
                    return Err(format!("dataset '{}': format applies only to path", dataset.name))
                }
                _ => return Err(format!("dataset '{}': set exactly one of path and sql", dataset.name)),
            }
        }
        Ok(config)
    }

    pub fn dataset_specs(&self) -> Vec<DatasetSpec> {
        self.datasets
            .iter()
            .map(|d| DatasetSpec {
                name: d.name.clone(),
                source: match (&d.path, &d.sql) {
                    (Some(path), _) => Source::File {
                        path: path.clone(),
                        format: d.format.map(|f| match f {
                            FormatConfig::Csv => Format::Csv,
                            FormatConfig::Parquet => Format::Parquet,
                        }),
                    },
                    (None, sql) => Source::Query(sql.clone().unwrap_or_default()),
                },
                key: d.key.clone(),
            })
            .collect()
    }
}

/// Refuses to start somewhere others can reach while nobody can sign in:
/// the first visitor would otherwise be offered the administrator account.
pub fn check_exposure(config: &Config, account_count: u64) -> Result<(), String> {
    let exposed = !config.bind.ip().is_loopback() || config.public_url.is_some();
    if exposed && account_count == 0 {
        return Err(format!(
            "no accounts exist yet, so Open Beam will not listen beyond this computer. \
             Create the administrator first: set {ADMIN_PASSWORD_ENV} and run `beam create-admin <username>`"
        ));
    }
    Ok(())
}
