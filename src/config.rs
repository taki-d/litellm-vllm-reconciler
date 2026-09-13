use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{collections::HashSet, time::Duration};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub litellm: LiteLlm,
    #[serde(default)]
    pub reconcile: Reconcile,
    pub servers: Vec<Server>,
    #[serde(default = "listen")]
    pub listen_address: String,
}
fn listen() -> String {
    "0.0.0.0:9090".into()
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiteLlm {
    pub base_url: String,
    pub master_key_env: String,
    #[serde(default = "timeout")]
    pub request_timeout_seconds: u64,
}
fn timeout() -> u64 {
    10
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Reconcile {
    pub interval_seconds: u64,
    pub failure_threshold: u64,
    pub deletion_grace_seconds: u64,
    pub startup_delay_seconds: u64,
    pub dry_run: bool,
}
impl Default for Reconcile {
    fn default() -> Self {
        Self {
            interval_seconds: 15,
            failure_threshold: 3,
            deletion_grace_seconds: 60,
            startup_delay_seconds: 5,
            dry_run: false,
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub id: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    #[serde(default = "enabled")]
    pub enabled: bool,
}
fn enabled() -> bool {
    true
}

pub fn secret(name: &str) -> Result<String> {
    let value = if let Ok(value) = std::env::var(name) {
        value
    } else {
        let path = std::env::var(format!("{name}_FILE")).with_context(|| {
            format!("secret environment variable {name} or {name}_FILE is required")
        })?;
        std::fs::read_to_string(path)
            .map_err(|_| anyhow::anyhow!("cannot read secret file for {name}"))?
            .trim_end()
            .to_owned()
    };
    ensure!(
        !value.is_empty() && !value.contains(['\r', '\n']),
        "invalid secret for {name}"
    );
    Ok(value)
}
impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("cannot read configuration")?;
        // Parser diagnostics can echo secrets accidentally pasted into YAML.
        let mut config: Self = serde_yaml::from_str(&text)
            .map_err(|_| anyhow::anyhow!("invalid configuration YAML or unknown field"))?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&mut self) -> Result<()> {
        validate_url(&mut self.litellm.base_url)?;
        ensure!(
            self.litellm.request_timeout_seconds > 0 && self.litellm.request_timeout_seconds <= 300,
            "request timeout must be 1..300 seconds"
        );
        ensure!(
            self.reconcile.interval_seconds > 0 && self.reconcile.failure_threshold > 0,
            "interval and failure threshold must be positive"
        );
        self.listen_address
            .parse::<std::net::SocketAddr>()
            .context("invalid listen_address")?;
        validate_env(&self.litellm.master_key_env)?;
        let mut ids = HashSet::new();
        for server in &mut self.servers {
            ensure!(
                !server.id.is_empty()
                    && server
                        .id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
                "source id must use letters, digits, dash, underscore or dot"
            );
            ensure!(ids.insert(server.id.clone()), "duplicate source id");
            validate_url(&mut server.base_url)?;
            if let Some(key) = &server.api_key_env {
                validate_env(key)?;
            }
        }
        Ok(())
    }
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.litellm.request_timeout_seconds)
    }
}
fn validate_env(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "invalid secret environment variable name"
    );
    Ok(())
}
fn validate_url(value: &mut String) -> Result<()> {
    let url = reqwest::Url::parse(value).map_err(|_| anyhow::anyhow!("invalid base URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "base URL must be HTTP(S) without credentials, query or fragment"
    );
    *value = value.trim_end_matches('/').to_owned();
    Ok(())
}
