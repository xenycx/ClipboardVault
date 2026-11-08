//! `clipboard-vault import-postgres`: one-time copy of a 2.x installation
//! (PostgreSQL + Better Auth + the uploads volume) into a 3.0 data directory.
//!
//! The source database is only read. Every ID is kept, so item URLs and API key
//! IDs stay valid; password hashes and API-key hashes are copied as-is (3.0 verifies
//! both formats). Sessions, Better Auth verification and invitation rows, and Tus
//! upload sessions are not imported: everyone signs in once, and in-flight
//! resumable uploads restart.
//!
//! Order of work:
//! 1. Refuse if the target database already has users.
//! 2. Copy blob files (when `--uploads-from` is given), keeping each `storage_key`,
//!    skipping files already present with the right size. No database lock is held.
//! 3. Import every table in one SQLite transaction.
//! 4. Check that every blob row has its file with the recorded size, and that each
//!    table's row count matches the source. Any mismatch exits non-zero.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use futures_util::TryStreamExt;
use serde_json::Value;
use sqlx::{
    PgPool, Row, SqliteConnection,
    postgres::{PgPoolOptions, PgRow},
};
use uuid::Uuid;

use crate::db::{self, Db};

const HELP: &str = "\
Copy a Clipboard Vault 2.x installation into a 3.0 data directory.

Usage:
  clipboard-vault import-postgres --from <postgres-url> --data-dir <dir>
                                  [--uploads-from <dir>] [--dry-run]

Options:
  --from          PostgreSQL URL of the 2.x database (it is only read)
  --data-dir      3.0 data directory; vault.db and uploads/ are created here
  --uploads-from  2.x uploads directory to copy files from (storage keys are kept)
  --dry-run       Import into a temporary database, check files, report, keep nothing

The import refuses to run when the target database already has users.
";

#[derive(Debug, Clone)]
pub struct Options {
    pub from: String,
    pub data_dir: PathBuf,
    pub uploads_from: Option<PathBuf>,
    pub dry_run: bool,
}

impl Options {
    pub fn parse(args: &[String]) -> anyhow::Result<Option<Self>> {
        let mut from = None;
        let mut data_dir = None;
        let mut uploads_from = None;
        let mut dry_run = false;
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (arg.as_str(), None),
            };
            let mut value = || {
                inline
                    .clone()
                    .or_else(|| iter.next().cloned())
                    .with_context(|| format!("{name} needs a value"))
            };
            match name {
                "--from" => from = Some(value()?),
                "--data-dir" => data_dir = Some(PathBuf::from(value()?)),
                "--uploads-from" => uploads_from = Some(PathBuf::from(value()?)),
                "--dry-run" => dry_run = true,
                "--help" | "-h" => return Ok(None),
                other => bail!("unknown option {other:?}\n\n{HELP}"),
            }
        }
        Ok(Some(Self {
            from: from.context("--from is required")?,
            data_dir: data_dir.context("--data-dir is required")?,
            uploads_from,
            dry_run,
        }))
    }
}

/// Source and target row counts for one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Count {
    pub table: &'static str,
    pub source: i64,
    pub target: i64,
}

#[derive(Debug, Default)]
pub struct Report {
    pub counts: Vec<Count>,
    /// Blob files checked on disk with the recorded size (`None`: not checked).
    pub files_verified: Option<u64>,
    pub files_copied: u64,
    pub files_present: u64,
    pub bytes_copied: u64,
    pub file_problems: Vec<String>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.file_problems.is_empty()
            && self.counts.iter().all(|count| count.source == count.target)
    }

    pub fn render(&self) -> String {
        let mut out = String::from("table                     source   target\n");
        for count in &self.counts {
            let flag = if count.source == count.target {
                ""
            } else {
                "  MISMATCH"
            };
            out.push_str(&format!(
                "{:<24} {:>7}  {:>7}{flag}\n",
                count.table, count.source, count.target
            ));
        }
        match self.files_verified {
            Some(verified) => out.push_str(&format!(
                "files: {verified} verified; {} copied ({} bytes), {} already present\n",
                self.files_copied, self.bytes_copied, self.files_present
            )),
            None => out.push_str("files: not checked (pass --uploads-from to check them)\n"),
        }
        for problem in &self.file_problems {
            out.push_str(&format!("FILE PROBLEM: {problem}\n"));
        }
        out
    }
}

pub async fn run(args: &[String]) -> anyhow::Result<()> {
    let Some(options) = Options::parse(args)? else {
        print!("{HELP}");
        return Ok(());
    };
    let report = import(&options).await?;
    print!("{}", report.render());
    if !report.is_clean() {
        bail!(
            "the import found mismatches; nothing should be switched over until they are resolved"
        );
    }
    if options.dry_run {
        println!(
            "dry run complete: nothing was written to {}",
            options.data_dir.display()
        );
    } else {
        println!(
            "import complete: {}",
            options.data_dir.join(db::DATABASE_FILE).display()
        );
    }
    Ok(())
}

pub async fn import(options: &Options) -> anyhow::Result<Report> {
    let source = PgPoolOptions::new()
        .max_connections(2)
        .connect(&options.from)
        .await
        .context("connect to the 2.x PostgreSQL database")?;
    let upload_root = options.data_dir.join("uploads");

    let scratch = options
        .dry_run
        .then(|| std::env::temp_dir().join(format!("clipboard-vault-import-{}", Uuid::new_v4())));
    let database_dir = scratch.clone().unwrap_or_else(|| options.data_dir.clone());
    tokio::fs::create_dir_all(&database_dir)
        .await
        .with_context(|| format!("create {}", database_dir.display()))?;
    let result = async {
        let target = Db::open(&database_dir, 1).await?;
        let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
            .fetch_one(&target.read)
            .await?;
        if users > 0 {
            bail!(
                "{} already has {users} user(s); the import only fills an empty 3.0 database",
                target.path().display()
            );
        }

        let mut report = Report::default();
        let blobs = blob_manifest(&source).await?;
        if let Some(uploads_from) = &options.uploads_from {
            copy_files(
                uploads_from,
                &upload_root,
                &blobs,
                options.dry_run,
                &mut report,
            )
            .await?;
        }

        let mut tx = target.begin().await?;
        // Rows reference each other in cycles (users approve users, keys belong to
        // workspaces that items reference), so check foreign keys at commit.
        sqlx::query("PRAGMA defer_foreign_keys = ON")
            .execute(&mut *tx)
            .await?;
        let counts = copy_tables(&source, &mut tx).await?;
        tx.commit().await.context("commit the import")?;
        report.counts = counts;

        let verify_root = if options.dry_run {
            options.uploads_from.clone()
        } else {
            Some(upload_root.clone())
        };
        if let Some(root) = verify_root {
            verify_files(&root, &blobs, &mut report).await;
        }
        target.read.close().await;
        target.write.close().await;
        Ok(report)
    }
    .await;
    if let Some(scratch) = scratch {
        let _ = tokio::fs::remove_dir_all(scratch).await;
    }
    source.close().await;
    result
}

struct BlobFile {
    storage_key: String,
    size: i64,
}

async fn blob_manifest(source: &PgPool) -> anyhow::Result<Vec<BlobFile>> {
    Ok(
        sqlx::query("SELECT storage_key, size_bytes FROM vault_blobs ORDER BY storage_key")
            .fetch_all(source)
            .await
            .context("read vault_blobs")?
            .into_iter()
            .map(|row| BlobFile {
                storage_key: row.get("storage_key"),
                size: row.get("size_bytes"),
            })
            .collect(),
    )
}

/// Accepts only plain relative paths (`org/ab/cd/<uuid>`), never `..` or absolute.
fn safe_relative(key: &str) -> Option<&Path> {
    let path = Path::new(key);
    path.components()
        .all(|part| matches!(part, std::path::Component::Normal(_)))
        .then_some(path)
}

async fn copy_files(
    from: &Path,
    to: &Path,
    blobs: &[BlobFile],
    dry_run: bool,
    report: &mut Report,
) -> anyhow::Result<()> {
    for blob in blobs {
        let Some(relative) = safe_relative(&blob.storage_key) else {
            report
                .file_problems
                .push(format!("unsafe storage key {:?}", blob.storage_key));
            continue;
        };
        let source = from.join(relative);
        let size = match tokio::fs::metadata(&source).await {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                report
                    .file_problems
                    .push(format!("{}: {error}", source.display()));
                continue;
            }
        };
        if size != blob.size as u64 {
            report.file_problems.push(format!(
                "{}: {size} bytes on disk, {} recorded",
                source.display(),
                blob.size
            ));
            continue;
        }
        if dry_run {
            continue;
        }
        let target = to.join(relative);
        if tokio::fs::metadata(&target)
            .await
            .is_ok_and(|metadata| metadata.len() == size)
        {
            report.files_present += 1;
            continue;
        }
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let partial = target.with_extension("import-partial");
        tokio::fs::copy(&source, &partial)
            .await
            .with_context(|| format!("copy {}", source.display()))?;
        tokio::fs::rename(&partial, &target).await?;
        report.files_copied += 1;
        report.bytes_copied += size;
    }
    if !dry_run {
        tokio::fs::create_dir_all(to.join("tmp")).await?;
    }
    Ok(())
}

async fn verify_files(root: &Path, blobs: &[BlobFile], report: &mut Report) {
    let mut verified = 0;
    for blob in blobs {
        let Some(relative) = safe_relative(&blob.storage_key) else {
            continue;
        };
        let path = root.join(relative);
        match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.len() == blob.size as u64 => verified += 1,
            Ok(metadata) => report.file_problems.push(format!(
                "{}: {} bytes, {} recorded",
                path.display(),
                metadata.len(),
                blob.size
            )),
            Err(_) => {
                let message = format!("{} is missing", path.display());
                if !report.file_problems.contains(&message) {
                    report.file_problems.push(message);
                }
            }
        }
    }
    report.files_verified = Some(verified);
}

fn ts(value: DateTime<Utc>) -> String {
    db::ts(value)
}

fn opt_ts(value: Option<DateTime<Utc>>) -> Option<String> {
    value.map(db::ts)
}

fn uuid_text(value: Option<Uuid>) -> Option<String> {
    value.map(|id| id.hyphenated().to_string())
}

async fn table_exists(source: &PgPool, name: &str) -> anyhow::Result<bool> {
    Ok(
        sqlx::query_scalar::<_, bool>("SELECT to_regclass($1) IS NOT NULL")
            .bind(name)
            .fetch_one(source)
            .await?,
    )
}

async fn source_count(source: &PgPool, sql: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(sql).fetch_one(source).await?)
}

async fn target_count(target: &mut SqliteConnection, table: &str) -> anyhow::Result<i64> {
    Ok(
        sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&mut *target)
            .await?,
    )
}

fn key_metadata_creator(value: Option<String>) -> Option<String> {
    let metadata: Value = serde_json::from_str(&value?).ok()?;
    // Better Auth sometimes double-encodes metadata as a JSON string.
    let metadata = match metadata {
        Value::String(inner) => serde_json::from_str(&inner).ok()?,
        other => other,
    };
    metadata
        .get("createdByUserId")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn normalize_permissions(value: Option<String>) -> String {
    let parsed = value
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .map(|value| match value {
            Value::String(inner) => serde_json::from_str(&inner).unwrap_or(Value::Null),
            other => other,
        });
    match parsed {
        Some(value @ Value::Object(_)) => value.to_string(),
        _ => "{}".into(),
    }
}

async fn copy_tables(source: &PgPool, target: &mut SqliteConnection) -> anyhow::Result<Vec<Count>> {
    let mut counts = Vec::new();
    let now = Utc::now();

    // Users, with their email/password credential folded in.
    let mut rows = sqlx::query(
        r#"SELECT u.id, u.name, lower(u.email) AS email, u."emailVerified" AS email_verified, u.image,
                  coalesce(u.role, 'user') AS role, coalesce(u."approvalStatus", 'pending') AS approval_status,
                  u."approvedAt" AS approved_at, u."approvedBy" AS approved_by,
                  coalesce(u.banned, false) AS banned, u."banReason" AS ban_reason, u."banExpires" AS ban_expires,
                  u."createdAt" AS created_at, u."updatedAt" AS updated_at,
                  (SELECT a.password FROM account a
                   WHERE a."userId" = u.id AND a."providerId" = 'credential' AND a.password IS NOT NULL
                   ORDER BY a."updatedAt" DESC LIMIT 1) AS password_hash
           FROM "user" u ORDER BY u."createdAt""#,
    )
    .fetch(source);
    while let Some(row) = rows.try_next().await.context("read users")? {
        let ban_expires: Option<DateTime<Utc>> = row.try_get("ban_expires")?;
        // Better Auth lifts a ban whose expiry has passed at the next sign-in.
        let banned =
            row.try_get::<bool, _>("banned")? && ban_expires.is_none_or(|expiry| expiry > now);
        sqlx::query(
            r#"INSERT INTO users (id, name, email, email_verified, image, password_hash, role,
                approval_status, approved_at, approved_by, banned, ban_reason, created_at, updated_at)
               VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)"#,
        )
        .bind(row.try_get::<String, _>("id")?)
        .bind(row.try_get::<String, _>("name")?)
        .bind(row.try_get::<String, _>("email")?)
        .bind(row.try_get::<bool, _>("email_verified")?)
        .bind(row.try_get::<Option<String>, _>("image")?)
        .bind(row.try_get::<Option<String>, _>("password_hash")?)
        .bind(row.try_get::<String, _>("role")?)
        .bind(row.try_get::<String, _>("approval_status")?)
        .bind(opt_ts(row.try_get("approved_at")?))
        .bind(row.try_get::<Option<String>, _>("approved_by")?)
        .bind(banned)
        .bind(banned.then(|| row.try_get::<Option<String>, _>("ban_reason").ok().flatten()).flatten())
        .bind(ts(row.try_get("created_at")?))
        .bind(ts(row.try_get("updated_at")?))
        .execute(&mut *target)
        .await
        .context("insert user")?;
    }
    drop(rows);
    counts.push(Count {
        table: "users",
        source: source_count(source, r#"SELECT count(*) FROM "user""#).await?,
        target: target_count(target, "users").await?,
    });

    let mut rows = sqlx::query(
        r#"SELECT id, "userId" AS user_id, "providerId" AS provider, "accountId" AS account_id,
                  "createdAt" AS created_at
           FROM account WHERE "providerId" <> 'credential'"#,
    )
    .fetch(source);
    while let Some(row) = rows.try_next().await.context("read social accounts")? {
        sqlx::query(
            "INSERT INTO oauth_accounts (id, user_id, provider, provider_account_id, created_at) VALUES (?,?,?,?,?)",
        )
        .bind(row.try_get::<String, _>("id")?)
        .bind(row.try_get::<String, _>("user_id")?)
        .bind(row.try_get::<String, _>("provider")?)
        .bind(row.try_get::<String, _>("account_id")?)
        .bind(ts(row.try_get("created_at")?))
        .execute(&mut *target)
        .await
        .context("insert social account")?;
    }
    drop(rows);
    counts.push(Count {
        table: "oauth_accounts",
        source: source_count(
            source,
            r#"SELECT count(*) FROM account WHERE "providerId" <> 'credential'"#,
        )
        .await?,
        target: target_count(target, "oauth_accounts").await?,
    });

    let mut rows =
        sqlx::query(r#"SELECT id, name, slug, "createdAt" AS created_at FROM organization"#)
            .fetch(source);
    while let Some(row) = rows.try_next().await.context("read organizations")? {
        sqlx::query("INSERT INTO workspaces (id, name, slug, created_at) VALUES (?,?,?,?)")
            .bind(row.try_get::<String, _>("id")?)
            .bind(row.try_get::<String, _>("name")?)
            .bind(row.try_get::<String, _>("slug")?)
            .bind(ts(row.try_get("created_at")?))
            .execute(&mut *target)
            .await
            .context("insert workspace")?;
    }
    drop(rows);
    counts.push(Count {
        table: "workspaces",
        source: source_count(source, "SELECT count(*) FROM organization").await?,
        target: target_count(target, "workspaces").await?,
    });

    let mut rows = sqlx::query(
        r#"SELECT id, "organizationId" AS workspace_id, "userId" AS user_id, role, "createdAt" AS created_at FROM member"#,
    )
    .fetch(source);
    while let Some(row) = rows.try_next().await.context("read members")? {
        sqlx::query("INSERT INTO memberships (id, workspace_id, user_id, role, created_at) VALUES (?,?,?,?,?)")
            .bind(row.try_get::<String, _>("id")?)
            .bind(row.try_get::<String, _>("workspace_id")?)
            .bind(row.try_get::<String, _>("user_id")?)
            .bind(row.try_get::<String, _>("role")?)
            .bind(ts(row.try_get("created_at")?))
            .execute(&mut *target)
            .await
            .context("insert membership")?;
    }
    drop(rows);
    counts.push(Count {
        table: "memberships",
        source: source_count(source, "SELECT count(*) FROM member").await?,
        target: target_count(target, "memberships").await?,
    });

    let mut rows = sqlx::query(
        r#"SELECT k.id, k."referenceId" AS workspace_id, k.name, k.start, k.key, k.permissions, k.metadata,
                  k.enabled, k."expiresAt" AS expires_at, k."requestCount" AS request_count,
                  k."lastRequest" AS last_request_at, k."createdAt" AS created_at, k."updatedAt" AS updated_at
           FROM apikey k"#,
    )
    .fetch(source);
    let mut keys = Vec::new();
    while let Some(row) = rows.try_next().await.context("read API keys")? {
        keys.push(row);
    }
    drop(rows);
    for row in keys {
        insert_api_key(&row, target).await?;
    }
    counts.push(Count {
        table: "api_keys",
        source: source_count(source, "SELECT count(*) FROM apikey").await?,
        target: target_count(target, "api_keys").await?,
    });

    let mut rows = sqlx::query(
        "SELECT id, organization_id, sha256, storage_key, size_bytes, mime_type, created_at FROM vault_blobs",
    )
    .fetch(source);
    while let Some(row) = rows.try_next().await.context("read blobs")? {
        sqlx::query(
            "INSERT INTO vault_blobs (id, organization_id, sha256, storage_key, size_bytes, mime_type, created_at) VALUES (?,?,?,?,?,?,?)",
        )
        .bind(uuid_text(Some(row.try_get("id")?)))
        .bind(row.try_get::<String, _>("organization_id")?)
        .bind(row.try_get::<String, _>("sha256")?)
        .bind(row.try_get::<String, _>("storage_key")?)
        .bind(row.try_get::<i64, _>("size_bytes")?)
        .bind(row.try_get::<String, _>("mime_type")?)
        .bind(ts(row.try_get("created_at")?))
        .execute(&mut *target)
        .await
        .context("insert blob")?;
    }
    drop(rows);
    counts.push(Count {
        table: "vault_blobs",
        source: source_count(source, "SELECT count(*) FROM vault_blobs").await?,
        target: target_count(target, "vault_blobs").await?,
    });

    // Items are streamed one row at a time: text payloads can be large.
    let mut rows = sqlx::query(
        r#"SELECT id, organization_id, created_by_user_id, created_by_key_id, kind, text_payload, blob_id,
                  original_filename, virtual_path, source_url, tags, content_hash, size_bytes, pinned,
                  created_at, updated_at, deleted_at
           FROM vault_items ORDER BY created_at, id"#,
    )
    .fetch(source);
    while let Some(row) = rows.try_next().await.context("read items")? {
        insert_item(&row, target).await?;
    }
    drop(rows);
    counts.push(Count {
        table: "vault_items",
        source: source_count(source, "SELECT count(*) FROM vault_items").await?,
        target: target_count(target, "vault_items").await?,
    });

    let mut rows = sqlx::query(
        r#"SELECT id, organization_id, token_hash, role, created_by_user_id, expires_at, redeemed_at,
                  redeemed_by_user_id, created_at FROM vault_invitations"#,
    )
    .fetch(source);
    while let Some(row) = rows.try_next().await.context("read invitations")? {
        sqlx::query(
            r#"INSERT INTO vault_invitations (id, organization_id, token_hash, role, created_by_user_id,
                expires_at, redeemed_at, redeemed_by_user_id, created_at) VALUES (?,?,?,?,?,?,?,?,?)"#,
        )
        .bind(uuid_text(Some(row.try_get("id")?)))
        .bind(row.try_get::<String, _>("organization_id")?)
        .bind(row.try_get::<String, _>("token_hash")?)
        .bind(row.try_get::<String, _>("role")?)
        .bind(row.try_get::<String, _>("created_by_user_id")?)
        .bind(ts(row.try_get("expires_at")?))
        .bind(opt_ts(row.try_get("redeemed_at")?))
        .bind(row.try_get::<Option<String>, _>("redeemed_by_user_id")?)
        .bind(ts(row.try_get("created_at")?))
        .execute(&mut *target)
        .await
        .context("insert invitation")?;
    }
    drop(rows);
    counts.push(Count {
        table: "vault_invitations",
        source: source_count(source, "SELECT count(*) FROM vault_invitations").await?,
        target: target_count(target, "vault_invitations").await?,
    });

    let mut rows = sqlx::query(
        r#"SELECT id, organization_id, actor_user_id, actor_key_id, action, item_id, detail, ip_hash, created_at
           FROM vault_activity ORDER BY id"#,
    )
    .fetch(source);
    while let Some(row) = rows.try_next().await.context("read activity")? {
        sqlx::query(
            r#"INSERT INTO vault_activity (id, organization_id, actor_user_id, actor_key_id, action, item_id,
                detail, ip_hash, created_at) VALUES (?,?,?,?,?,?,?,?,?)"#,
        )
        .bind(row.try_get::<i64, _>("id")?)
        .bind(row.try_get::<Option<String>, _>("organization_id")?)
        .bind(row.try_get::<Option<String>, _>("actor_user_id")?)
        .bind(row.try_get::<Option<String>, _>("actor_key_id")?)
        .bind(row.try_get::<String, _>("action")?)
        .bind(uuid_text(row.try_get("item_id")?))
        .bind(row.try_get::<Value, _>("detail")?.to_string())
        .bind(row.try_get::<Option<String>, _>("ip_hash")?)
        .bind(ts(row.try_get("created_at")?))
        .execute(&mut *target)
        .await
        .context("insert activity")?;
    }
    drop(rows);
    counts.push(Count {
        table: "vault_activity",
        source: source_count(source, "SELECT count(*) FROM vault_activity").await?,
        target: target_count(target, "vault_activity").await?,
    });

    let mut rows = sqlx::query(
        "SELECT organization_id, max_upload_bytes, retention_days, updated_at, updated_by_user_id FROM vault_workspace_settings",
    )
    .fetch(source);
    while let Some(row) = rows.try_next().await.context("read workspace settings")? {
        sqlx::query(
            r#"INSERT INTO vault_workspace_settings (organization_id, max_upload_bytes, retention_days,
                updated_at, updated_by_user_id) VALUES (?,?,?,?,?)"#,
        )
        .bind(row.try_get::<String, _>("organization_id")?)
        .bind(row.try_get::<i64, _>("max_upload_bytes")?)
        .bind(row.try_get::<i32, _>("retention_days")?)
        .bind(ts(row.try_get("updated_at")?))
        .bind(row.try_get::<Option<String>, _>("updated_by_user_id")?)
        .execute(&mut *target)
        .await
        .context("insert workspace settings")?;
    }
    drop(rows);
    counts.push(Count {
        table: "vault_workspace_settings",
        source: source_count(source, "SELECT count(*) FROM vault_workspace_settings").await?,
        target: target_count(target, "vault_workspace_settings").await?,
    });

    // Server settings arrived in 2.x migration 0003; older installs keep the defaults.
    if table_exists(source, "vault_server_settings").await? {
        let row = sqlx::query(
            r#"SELECT registration_enabled, require_approval, default_max_upload_bytes, updated_at,
                      updated_by_user_id FROM vault_server_settings WHERE id"#,
        )
        .fetch_optional(source)
        .await?;
        if let Some(row) = row {
            sqlx::query(
                r#"UPDATE server_settings SET registration_enabled = ?, require_approval = ?,
                   default_max_upload_bytes = ?, updated_at = ?, updated_by_user_id = ? WHERE id = 1"#,
            )
            .bind(row.try_get::<bool, _>("registration_enabled")?)
            .bind(row.try_get::<bool, _>("require_approval")?)
            .bind(row.try_get::<i64, _>("default_max_upload_bytes")?)
            .bind(ts(row.try_get("updated_at")?))
            .bind(row.try_get::<Option<String>, _>("updated_by_user_id")?)
            .execute(&mut *target)
            .await
            .context("update server settings")?;
        }
    }
    Ok(counts)
}

async fn insert_api_key(row: &PgRow, target: &mut SqliteConnection) -> anyhow::Result<()> {
    let creator = key_metadata_creator(row.try_get("metadata")?);
    // A creator who has since been deleted leaves the key ownerless, as in 2.x.
    let creator = match creator {
        Some(id) => {
            sqlx::query_scalar::<_, String>("SELECT id FROM users WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *target)
                .await?
        }
        None => None,
    };
    sqlx::query(
        r#"INSERT INTO api_keys (id, workspace_id, created_by_user_id, name, start, key_hash, permissions,
            enabled, expires_at, request_count, last_request_at, created_at, updated_at)
           VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)"#,
    )
    .bind(row.try_get::<String, _>("id")?)
    .bind(row.try_get::<String, _>("workspace_id")?)
    .bind(creator)
    .bind(row.try_get::<Option<String>, _>("name")?)
    .bind(row.try_get::<Option<String>, _>("start")?)
    .bind(row.try_get::<String, _>("key")?)
    .bind(normalize_permissions(row.try_get("permissions")?))
    .bind(row.try_get::<bool, _>("enabled")?)
    .bind(opt_ts(row.try_get("expires_at")?))
    .bind(i64::from(row.try_get::<i32, _>("request_count")?))
    .bind(opt_ts(row.try_get("last_request_at")?))
    .bind(ts(row.try_get("created_at")?))
    .bind(ts(row.try_get("updated_at")?))
    .execute(&mut *target)
    .await
    .context("insert API key")?;
    Ok(())
}

async fn insert_item(row: &PgRow, target: &mut SqliteConnection) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO vault_items (id, organization_id, created_by_user_id, created_by_key_id, kind,
            text_payload, blob_id, original_filename, virtual_path, source_url, tags, content_hash,
            size_bytes, pinned, created_at, updated_at, deleted_at)
           VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"#,
    )
    .bind(uuid_text(Some(row.try_get("id")?)))
    .bind(row.try_get::<String, _>("organization_id")?)
    .bind(row.try_get::<Option<String>, _>("created_by_user_id")?)
    .bind(row.try_get::<Option<String>, _>("created_by_key_id")?)
    .bind(row.try_get::<String, _>("kind")?)
    .bind(row.try_get::<Option<String>, _>("text_payload")?)
    .bind(uuid_text(row.try_get("blob_id")?))
    .bind(row.try_get::<Option<String>, _>("original_filename")?)
    .bind(row.try_get::<String, _>("virtual_path")?)
    .bind(row.try_get::<Option<String>, _>("source_url")?)
    .bind(row.try_get::<Value, _>("tags")?.to_string())
    .bind(row.try_get::<String, _>("content_hash")?)
    .bind(row.try_get::<i64, _>("size_bytes")?)
    .bind(row.try_get::<bool, _>("pinned")?)
    .bind(ts(row.try_get("created_at")?))
    .bind(ts(row.try_get("updated_at")?))
    .bind(opt_ts(row.try_get("deleted_at")?))
    .execute(&mut *target)
    .await
    .context("insert item")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_both_spellings() {
        let args: Vec<String> = ["--from", "postgres://x", "--data-dir=/data", "--dry-run"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        let options = Options::parse(&args).unwrap().unwrap();
        assert_eq!(options.from, "postgres://x");
        assert_eq!(options.data_dir, PathBuf::from("/data"));
        assert!(options.dry_run);
        assert!(options.uploads_from.is_none());
        assert!(Options::parse(&["--data-dir".into(), "/d".into()]).is_err());
        assert!(Options::parse(&["--help".into()]).unwrap().is_none());
    }

    #[test]
    fn key_metadata_and_permissions_tolerate_double_encoding() {
        assert_eq!(
            key_metadata_creator(Some(r#"{"createdByUserId":"u1"}"#.into())).as_deref(),
            Some("u1")
        );
        assert_eq!(
            key_metadata_creator(Some(r#""{\"createdByUserId\":\"u2\"}""#.into())).as_deref(),
            Some("u2")
        );
        assert_eq!(key_metadata_creator(None), None);
        assert_eq!(
            normalize_permissions(Some(r#"{"items":["write"]}"#.into())),
            r#"{"items":["write"]}"#
        );
        assert_eq!(
            normalize_permissions(Some(r#""{\"items\":[\"read\"]}""#.into())),
            r#"{"items":["read"]}"#
        );
        assert_eq!(normalize_permissions(Some("garbage".into())), "{}");
    }

    #[test]
    fn storage_keys_must_stay_relative() {
        assert!(safe_relative("org/ab/cd/id").is_some());
        assert!(safe_relative("../etc/passwd").is_none());
        assert!(safe_relative("/abs").is_none());
    }
}
