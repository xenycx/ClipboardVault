//! The embedded SQLite database.
//!
//! One file, `DATA_DIR/vault.db`, in WAL mode. Two pools share it:
//!
//! * `read`: several connections with `query_only` set, for every SELECT.
//! * `write`: exactly one connection. Every mutation goes through it, and every
//!   multi-statement change runs in `BEGIN IMMEDIATE` ([`Db::begin`]). With a single
//!   writer there is no lock upgrade to fail, which is what replaces the 2.x
//!   `SELECT … FOR UPDATE` and `pg_advisory_xact_lock` calls.
//!
//! Keep write transactions short. Never hash, stream or copy a file while one is
//! open: the next writer waits for the connection.

use std::{
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use anyhow::Context;
use chrono::{DateTime, Utc};
use sqlx::{
    Sqlite, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

/// Hyphenated UUIDs are stored as TEXT. sqlx would store a bare `Uuid` as a BLOB.
pub type Id = uuid::fmt::Hyphenated;

pub const DATABASE_FILE: &str = "vault.db";

#[derive(Clone)]
pub struct Db {
    pub read: SqlitePool,
    pub write: SqlitePool,
    path: PathBuf,
}

impl Db {
    /// Opens (creating if needed) `data_dir/vault.db` and applies migrations.
    pub async fn open(data_dir: &Path, read_connections: u32) -> anyhow::Result<Self> {
        Self::open_file(&data_dir.join(DATABASE_FILE), read_connections).await
    }

    pub async fn open_file(path: &Path, read_connections: u32) -> anyhow::Result<Self> {
        let options = connect_options(path)?;
        let write = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(options.clone().create_if_missing(true))
            .await
            .with_context(|| format!("open {}", path.display()))?;
        let mut migrator = sqlx::migrate!();
        // An older 3.x binary must still start on a database a newer one migrated,
        // so an image rollback works. Migrations are additive for that reason.
        migrator.set_ignore_missing(true);
        migrator
            .run(&write)
            .await
            .context("run database migrations")?;
        let read = SqlitePoolOptions::new()
            .max_connections(read_connections.max(1))
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(options.pragma("query_only", "ON"))
            .await
            .context("open read connections")?;
        Ok(Self {
            read,
            write,
            path: path.to_owned(),
        })
    }

    /// Starts a write transaction that takes the database write lock immediately.
    pub async fn begin(&self) -> sqlx::Result<Transaction<'static, Sqlite>> {
        self.write.begin_with("BEGIN IMMEDIATE").await
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes a consistent copy of the live database to `destination`.
    pub async fn snapshot(&self, destination: &Path) -> anyhow::Result<()> {
        snapshot_file(&self.path, destination).await
    }
}

/// Copies the database at `source` to `destination` with `VACUUM INTO`, on a
/// dedicated connection so no pool is held. The copy is a complete, consistent
/// database even while the server keeps writing. `destination` must not exist.
pub async fn snapshot_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    use sqlx::{ConnectOptions, Connection};
    let target = destination
        .to_str()
        .context("snapshot path must be valid UTF-8")?;
    if target.contains('\'') {
        anyhow::bail!("snapshot path must not contain quotes");
    }
    if !source.exists() {
        anyhow::bail!("{} does not exist", source.display());
    }
    let mut connection = connect_options(source)?
        .connect()
        .await
        .context("open snapshot connection")?;
    sqlx::query(&format!("VACUUM INTO '{target}'"))
        .execute(&mut connection)
        .await
        .context("write database snapshot")?;
    connection.close().await.ok();
    Ok(())
}

fn connect_options(path: &Path) -> anyhow::Result<SqliteConnectOptions> {
    let url = format!("sqlite://{}", path.display());
    Ok(SqliteConnectOptions::from_str(&url)
        .with_context(|| format!("invalid database path {}", path.display()))?
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5)))
}

/// The one timestamp format stored in the database: fixed width, UTC, microseconds.
/// Fixed width keeps lexical order equal to time order, so `ORDER BY` and `<`
/// comparisons on TEXT columns are correct.
pub fn ts(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

pub fn now() -> String {
    ts(Utc::now())
}

/// Parses a stored timestamp (or any RFC 3339 value) back into UTC.
pub fn parse_ts(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::Db;

    /// A migrated database in a temporary directory. Keep the directory alive.
    pub async fn temp_db() -> (tempfile::TempDir, Db) {
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open(directory.path(), 4).await.unwrap();
        (directory, db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn timestamps_are_fixed_width_and_sort_like_time() {
        let early = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let later = early + chrono::Duration::microseconds(1);
        let much_later = early + chrono::Duration::days(400);
        assert_eq!(ts(early), "2026-01-02T03:04:05.000000Z");
        assert_eq!(ts(early).len(), ts(much_later).len());
        assert!(ts(early) < ts(later));
        assert!(ts(later) < ts(much_later));
        assert_eq!(parse_ts(&ts(later)), Some(later));
    }

    #[tokio::test]
    async fn migrations_apply_and_pragmas_hold() {
        let (_directory, db) = test_support::temp_db().await;
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&db.write)
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&db.read)
            .await
            .unwrap();
        assert_eq!(foreign_keys, 1);
        // The read pool refuses writes.
        assert!(
            sqlx::query("DELETE FROM server_settings")
                .execute(&db.read)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn snapshots_are_complete_databases() {
        let (directory, db) = test_support::temp_db().await;
        let copy = directory.path().join("copy.db");
        db.snapshot(&copy).await.unwrap();
        let restored = Db::open_file(&copy, 1).await.unwrap();
        let settings: i64 = sqlx::query_scalar("SELECT count(*) FROM server_settings")
            .fetch_one(&restored.read)
            .await
            .unwrap();
        assert_eq!(settings, 1);
    }
}
