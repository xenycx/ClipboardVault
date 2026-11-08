use std::{env, path::PathBuf};

use anyhow::{Context, bail};

/// Environment variables that 2.x needed and 3.0 no longer reads. Starting with one
/// of them set is allowed, but it is logged so a half-migrated `.env` is noticed.
pub const RETIRED_VARIABLES: [&str; 7] = [
    "DATABASE_URL",
    "BETTER_AUTH_SECRET",
    "BETTER_AUTH_URL",
    "AUTH_BRIDGE_SECRET",
    "AUTH_INTERNAL_URL",
    "AUTH_PROXY_ENABLED",
    "BOOTSTRAP_TOKEN",
];

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    /// Holds `vault.db`, `uploads/` and `backups/`.
    pub data_dir: PathBuf,
    pub upload_root: PathBuf,
    pub public_base_url: String,
    /// Signs short-lived browser state such as the social sign-in cookie.
    pub vault_secret: String,
    /// Claims the first administrator account on `/setup`.
    pub setup_token: String,
    /// Read the client address from `X-Forwarded-For`. Only enable behind a proxy
    /// that overwrites or appends that header; otherwise clients can spoof it.
    pub trust_proxy_headers: bool,
    pub database_read_connections: u32,
    pub backup_keep: usize,
    pub server_max_upload_bytes: u64,
    pub legacy_max_upload_bytes: u64,
    pub upload_disk_reserve_bytes: u64,
    pub tus_chunk_size_bytes: u64,
    pub tus_session_ttl_seconds: u64,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let vault_secret = required("VAULT_SECRET")?;
        if vault_secret.len() < 32 {
            bail!("VAULT_SECRET must contain at least 32 characters");
        }
        let setup_token = required("SETUP_TOKEN")?;
        if setup_token.len() < 32 {
            bail!("SETUP_TOKEN must contain at least 32 characters");
        }
        let data_dir: PathBuf = env::var("DATA_DIR")
            .unwrap_or_else(|_| "/data".into())
            .into();
        let upload_root = env::var("UPLOAD_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir.join("uploads"));
        let config = Self {
            port: parse("PORT", 8080)?,
            data_dir,
            upload_root,
            public_base_url: env::var("PUBLIC_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:8080".into())
                .trim_end_matches('/')
                .to_owned(),
            vault_secret,
            setup_token,
            trust_proxy_headers: flag("TRUST_PROXY_HEADERS")?,
            database_read_connections: parse("DATABASE_READ_CONNECTIONS", 8)?,
            backup_keep: parse("BACKUP_KEEP", 7)?,
            server_max_upload_bytes: parse("SERVER_MAX_UPLOAD_BYTES", 10_737_418_240)?,
            legacy_max_upload_bytes: parse("LEGACY_MAX_UPLOAD_BYTES", 104_857_600)?,
            upload_disk_reserve_bytes: parse("UPLOAD_DISK_RESERVE_BYTES", 21_474_836_480)?,
            tus_chunk_size_bytes: parse("TUS_CHUNK_SIZE_BYTES", 16_777_216)?,
            tus_session_ttl_seconds: parse("TUS_SESSION_TTL_SECONDS", 604_800)?,
        };
        url::Url::parse(&config.public_base_url)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
            .context("PUBLIC_BASE_URL must be an absolute http(s) URL")?;
        config.validate_limits()?;
        Ok(config)
    }

    pub fn validate_limits(&self) -> anyhow::Result<()> {
        if self.server_max_upload_bytes == 0 || self.server_max_upload_bytes > i64::MAX as u64 {
            bail!("SERVER_MAX_UPLOAD_BYTES must be between 1 and {}", i64::MAX);
        }
        if self.legacy_max_upload_bytes == 0
            || self.legacy_max_upload_bytes > self.server_max_upload_bytes
            || self.legacy_max_upload_bytes.saturating_add(1024 * 1024) > usize::MAX as u64
        {
            bail!(
                "LEGACY_MAX_UPLOAD_BYTES must fit the request platform and not exceed the server maximum"
            );
        }
        if self.tus_chunk_size_bytes == 0
            || self.tus_chunk_size_bytes > self.legacy_max_upload_bytes
            || self.tus_chunk_size_bytes > usize::MAX as u64
        {
            bail!(
                "TUS_CHUNK_SIZE_BYTES must be positive, fit memory, and not exceed the legacy request limit"
            );
        }
        if self.tus_session_ttl_seconds == 0 || self.tus_session_ttl_seconds > i64::MAX as u64 {
            bail!("TUS_SESSION_TTL_SECONDS is outside the supported range");
        }
        Ok(())
    }

    pub fn secure_cookies(&self) -> bool {
        self.public_base_url.starts_with("https://")
    }

    /// Retired 2.x variables that are still set, for a startup warning.
    pub fn retired_variables_present() -> Vec<&'static str> {
        RETIRED_VARIABLES
            .into_iter()
            .filter(|name| env::var_os(name).is_some())
            .collect()
    }
}

pub(crate) fn flag(name: &str) -> anyhow::Result<bool> {
    parse_flag(name, &env::var(name).unwrap_or_default())
}

fn parse_flag(name: &str, value: &str) -> anyhow::Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        other => bail!("{name} must be true or false, not {other:?}"),
    }
}

fn required(name: &str) -> anyhow::Result<String> {
    env::var(name).with_context(|| format!("missing required environment variable {name}"))
}

fn parse<T>(name: &str, default: T) -> anyhow::Result<T>
where
    T: std::str::FromStr + ToString,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .with_context(|| format!("invalid {name}"))
}

#[cfg(test)]
mod tests {
    use super::parse_flag;

    #[test]
    fn proxy_trust_defaults_to_off() {
        assert!(!parse_flag("TRUST_PROXY_HEADERS", "").unwrap());
        assert!(!parse_flag("TRUST_PROXY_HEADERS", "false").unwrap());
        assert!(parse_flag("TRUST_PROXY_HEADERS", "true").unwrap());
        assert!(parse_flag("TRUST_PROXY_HEADERS", "maybe").is_err());
    }
}
