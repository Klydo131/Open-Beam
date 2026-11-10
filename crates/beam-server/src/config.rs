//! `beam.toml` loading and validation.

use beam_engine::{DatasetSpec, Format, Source};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Environment variable holding the password for `[auth]`.
pub const PASSWORD_ENV: &str = "BEAM_PASSWORD";

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
    pub auth: Option<AuthConfig>,
    #[serde(default, rename = "dataset")]
    pub datasets: Vec<DatasetConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub username: String,
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

fn default_page_size() -> u64 {
    5000
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let mut config: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if config.datasets.is_empty() {
            return Err(format!("{}: define at least one [[dataset]]", path.display()));
        }
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

/// Username and password required on every request.
#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// Resolves credentials, refusing configurations that would expose data to
/// the network without a password.
pub fn credentials(config: &Config, password: Option<String>) -> Result<Option<Credentials>, String> {
    match (&config.auth, password.filter(|p| !p.is_empty())) {
        (Some(auth), Some(password)) => Ok(Some(Credentials {
            username: auth.username.clone(),
            password,
        })),
        (Some(_), None) => Err(format!("[auth] is configured but {PASSWORD_ENV} is not set")),
        (None, _) if config.bind.ip().is_loopback() => Ok(None),
        (None, _) => Err(format!(
            "refusing to listen on {} without [auth]; add [auth] and set {PASSWORD_ENV}, or bind to 127.0.0.1",
            config.bind
        )),
    }
}
