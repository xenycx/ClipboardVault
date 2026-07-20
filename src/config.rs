use std::{env, path::PathBuf};

use anyhow::{Context, bail};

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub database_url: String,
    pub database_max_connections: u32,
    pub upload_root: PathBuf,
    pub public_base_url: String,
    pub auth_internal_url: String,
    pub auth_bridge_secret: String,
    pub server_max_upload_bytes: u64,
    pub legacy_max_upload_bytes: u64,
    pub upload_disk_reserve_bytes: u64,
    pub tus_chunk_size_bytes: u64,
    pub tus_session_ttl_seconds: u64,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let auth_bridge_secret = required("AUTH_BRIDGE_SECRET")?;
        if auth_bridge_secret.len() < 32 {
            bail!("AUTH_BRIDGE_SECRET must contain at least 32 characters");
        }
        let config = Self {
            port: parse("PORT", 8080)?,
            database_url: required("DATABASE_URL")?,
            database_max_connections: parse("DATABASE_MAX_CONNECTIONS", 20)?,
            upload_root: env::var("UPLOAD_ROOT")
                .unwrap_or_else(|_| "/data/uploads".into())
                .into(),
            public_base_url: env::var("PUBLIC_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:8080".into())
                .trim_end_matches('/')
                .to_owned(),
            auth_internal_url: env::var("AUTH_INTERNAL_URL")
                .unwrap_or_else(|_| "http://auth:3001".into())
                .trim_end_matches('/')
                .to_owned(),
            auth_bridge_secret,
            server_max_upload_bytes: parse("SERVER_MAX_UPLOAD_BYTES", 10_737_418_240)?,
            legacy_max_upload_bytes: parse("LEGACY_MAX_UPLOAD_BYTES", 104_857_600)?,
            upload_disk_reserve_bytes: parse("UPLOAD_DISK_RESERVE_BYTES", 21_474_836_480)?,
            tus_chunk_size_bytes: parse("TUS_CHUNK_SIZE_BYTES", 16_777_216)?,
            tus_session_ttl_seconds: parse("TUS_SESSION_TTL_SECONDS", 604_800)?,
        };
        if config.server_max_upload_bytes == 0 || config.server_max_upload_bytes > i64::MAX as u64 {
            bail!("SERVER_MAX_UPLOAD_BYTES must be between 1 and {}", i64::MAX);
        }
        if config.legacy_max_upload_bytes == 0
            || config.legacy_max_upload_bytes > config.server_max_upload_bytes
            || config.legacy_max_upload_bytes.saturating_add(1024 * 1024) > usize::MAX as u64
        {
            bail!(
                "LEGACY_MAX_UPLOAD_BYTES must fit the request platform and not exceed the server maximum"
            );
        }
        if config.tus_chunk_size_bytes == 0
            || config.tus_chunk_size_bytes > config.legacy_max_upload_bytes
            || config.tus_chunk_size_bytes > usize::MAX as u64
        {
            bail!(
                "TUS_CHUNK_SIZE_BYTES must be positive, fit memory, and not exceed the legacy request limit"
            );
        }
        if config.tus_session_ttl_seconds == 0 || config.tus_session_ttl_seconds > i64::MAX as u64 {
            bail!("TUS_SESSION_TTL_SECONDS is outside the supported range");
        }
        Ok(config)
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
