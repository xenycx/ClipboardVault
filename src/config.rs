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
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let auth_bridge_secret = required("AUTH_BRIDGE_SECRET")?;
        if auth_bridge_secret.len() < 32 {
            bail!("AUTH_BRIDGE_SECRET must contain at least 32 characters");
        }
        Ok(Self {
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
            server_max_upload_bytes: parse("SERVER_MAX_UPLOAD_BYTES", 104_857_600)?,
        })
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
