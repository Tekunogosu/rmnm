//! rmnm's own SQLite store: settings, extra environment variables and the signed-in session.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Schema migrations, applied in order; `PRAGMA user_version` records how many have run.
const MIGRATIONS: &[&str] = &[
    r#"
    CREATE TABLE settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE env_vars (
        id      INTEGER PRIMARY KEY,
        name    TEXT NOT NULL UNIQUE,
        value   TEXT NOT NULL,
        enabled INTEGER NOT NULL DEFAULT 1
    );
    -- At most one signed-in account.
    CREATE TABLE session (
        id           INTEGER PRIMARY KEY CHECK (id = 1),
        email        TEXT NOT NULL,
        token        TEXT NOT NULL,
        expires_at   INTEGER NOT NULL,
        source       TEXT NOT NULL,
        validated_at INTEGER
    );
"#,
    r#"
    -- One row per install folder rmnm has patched.
    CREATE TABLE installs (
        dir               TEXT PRIMARY KEY,
        slug              TEXT NOT NULL,
        version           TEXT,
        patch_in_progress INTEGER NOT NULL DEFAULT 0,
        updated_at        INTEGER NOT NULL
    );
    -- Files known to match their manifest hash, with the size and mtime seen when they were
    -- verified; Patch skips re-hashing a file whose hash, size and mtime are all unchanged.
    CREATE TABLE installed_files (
        dir       TEXT NOT NULL REFERENCES installs(dir) ON DELETE CASCADE,
        path      TEXT NOT NULL,
        file_hash TEXT NOT NULL,
        size      INTEGER NOT NULL,
        mtime_ns  INTEGER NOT NULL,
        PRIMARY KEY (dir, path)
    );
"#,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRecord {
    pub slug: String,
    pub version: Option<String>,
    pub patch_in_progress: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFile {
    /// XXH3-128 as 32 hex digits.
    pub hash: String,
    pub size: i64,
    pub mtime_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub id: i64,
    pub name: String,
    pub value: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSource {
    Login,
    OfficialLauncher,
}

impl SessionSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::OfficialLauncher => "official_launcher",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "official_launcher" => Self::OfficialLauncher,
            _ => Self::Login,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct StoredSession {
    pub email: String,
    pub token: String,
    /// Unix seconds, from the token's `exp` claim.
    pub expires_at: i64,
    pub source: SessionSource,
}

impl std::fmt::Debug for StoredSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredSession")
            .field("email", &self.email)
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("source", &self.source)
            .finish()
    }
}

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Opens (creating if needed) the database at `path`. The directory is created 0700 and
    /// the file 0600 before SQLite touches it, since the file holds the session token.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on {}", path.display()))?;
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        // The patcher writes from its own connection while the UI keeps one open.
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)?;
        let applied: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(applied.max(0) as usize) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)
                .with_context(|| format!("applying schema migration {}", index + 1))?;
            tx.pragma_update(None, "user_version", index as i64 + 1)?;
            tx.commit()?;
        }
        Ok(Self { conn })
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn all_settings(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT key, value FROM settings")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn clear_setting(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM settings WHERE key = ?1", [key])?;
        Ok(())
    }

    pub fn env_vars(&self) -> Result<Vec<EnvVar>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, value, enabled FROM env_vars ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok(EnvVar {
                id: row.get(0)?,
                name: row.get(1)?,
                value: row.get(2)?,
                enabled: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_env_var(&self, name: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO env_vars (name, value) VALUES (?1, ?2)",
                params![name, value],
            )
            .with_context(|| format!("{name} is already defined"))?;
        Ok(())
    }

    pub fn update_env_var(&self, id: i64, name: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE env_vars SET name = ?2, value = ?3 WHERE id = ?1",
                params![id, name, value],
            )
            .with_context(|| format!("{name} is already defined"))?;
        Ok(())
    }

    pub fn set_env_var_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE env_vars SET enabled = ?2 WHERE id = ?1",
            params![id, enabled],
        )?;
        Ok(())
    }

    pub fn delete_env_var(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM env_vars WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn session(&self) -> Result<Option<StoredSession>> {
        Ok(self
            .conn
            .query_row(
                "SELECT email, token, expires_at, source FROM session WHERE id = 1",
                [],
                |row| {
                    Ok(StoredSession {
                        email: row.get(0)?,
                        token: row.get(1)?,
                        expires_at: row.get(2)?,
                        source: SessionSource::parse(&row.get::<_, String>(3)?),
                    })
                },
            )
            .optional()?)
    }

    pub fn save_session(&self, session: &StoredSession, validated_at: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session (id, email, token, expires_at, source, validated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                 email = excluded.email, token = excluded.token,
                 expires_at = excluded.expires_at, source = excluded.source,
                 validated_at = excluded.validated_at",
            params![
                session.email,
                session.token,
                session.expires_at,
                session.source.as_str(),
                validated_at
            ],
        )?;
        Ok(())
    }

    pub fn mark_session_validated(&self, validated_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE session SET validated_at = ?1 WHERE id = 1",
            [validated_at],
        )?;
        Ok(())
    }

    pub fn clear_session(&self) -> Result<()> {
        self.conn.execute("DELETE FROM session", [])?;
        Ok(())
    }

    pub fn install(&self, dir: &str) -> Result<Option<InstallRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT slug, version, patch_in_progress FROM installs WHERE dir = ?1",
                [dir],
                |row| {
                    Ok(InstallRecord {
                        slug: row.get(0)?,
                        version: row.get(1)?,
                        patch_in_progress: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Records that a patch of `dir` has started; the version is kept until it completes.
    pub fn begin_patch(&self, dir: &str, slug: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO installs (dir, slug, patch_in_progress, updated_at) VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(dir) DO UPDATE SET slug = excluded.slug, patch_in_progress = 1,
                                            updated_at = excluded.updated_at",
            params![dir, slug, now],
        )?;
        Ok(())
    }

    pub fn finish_patch(&self, dir: &str, version: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE installs SET version = ?2, patch_in_progress = 0, updated_at = ?3 WHERE dir = ?1",
            params![dir, version, now],
        )?;
        Ok(())
    }

    pub fn verified_files(&self, dir: &str) -> Result<HashMap<String, VerifiedFile>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, file_hash, size, mtime_ns FROM installed_files WHERE dir = ?1",
        )?;
        let rows = stmt.query_map([dir], |row| {
            Ok((
                row.get::<_, String>(0)?,
                VerifiedFile {
                    hash: row.get(1)?,
                    size: row.get(2)?,
                    mtime_ns: row.get(3)?,
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn record_verified_file(&self, dir: &str, path: &str, file: &VerifiedFile) -> Result<()> {
        self.conn.execute(
            "INSERT INTO installed_files (dir, path, file_hash, size, mtime_ns) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(dir, path) DO UPDATE SET file_hash = excluded.file_hash,
                 size = excluded.size, mtime_ns = excluded.mtime_ns",
            params![dir, path, file.hash, file.size, file.mtime_ns],
        )?;
        Ok(())
    }

    pub fn forget_verified_file(&self, dir: &str, path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM installed_files WHERE dir = ?1 AND path = ?2",
            params![dir, path],
        )?;
        Ok(())
    }

    pub fn forget_verified_files(&self, dir: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM installed_files WHERE dir = ?1", [dir])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(token: &str) -> StoredSession {
        StoredSession {
            email: "a@example.com".into(),
            token: token.into(),
            expires_at: 2_000_000_000,
            source: SessionSource::OfficialLauncher,
        }
    }

    #[test]
    fn migrations_are_idempotent_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("rmnm.db");
        Db::open(&path).unwrap().set_setting("k", "v").unwrap();
        let db = Db::open(&path).unwrap();
        assert_eq!(db.setting("k").unwrap().as_deref(), Some("v"));
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn session_round_trips_and_replaces() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.session().unwrap(), None);
        db.save_session(&session("one"), 1).unwrap();
        db.save_session(&session("two"), 2).unwrap();
        assert_eq!(db.session().unwrap(), Some(session("two")));
        db.clear_session().unwrap();
        assert_eq!(db.session().unwrap(), None);
    }

    #[test]
    fn patch_records_track_version_and_files() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.install("/g").unwrap(), None);
        db.begin_patch("/g", "mnm", 1).unwrap();
        let record = db.install("/g").unwrap().unwrap();
        assert!(record.patch_in_progress);
        assert_eq!(record.version, None);

        let file = VerifiedFile {
            hash: "ab".into(),
            size: 3,
            mtime_ns: 9,
        };
        db.record_verified_file("/g", "mnm.exe", &file).unwrap();
        db.finish_patch("/g", "publish-1", 2).unwrap();
        let record = db.install("/g").unwrap().unwrap();
        assert!(!record.patch_in_progress);
        assert_eq!(record.version.as_deref(), Some("publish-1"));
        assert_eq!(db.verified_files("/g").unwrap()["mnm.exe"], file);

        db.forget_verified_file("/g", "mnm.exe").unwrap();
        assert!(db.verified_files("/g").unwrap().is_empty());

        db.record_verified_file("/g", "a", &file).unwrap();
        db.record_verified_file("/g", "b", &file).unwrap();
        db.begin_patch("/h", "mnm", 3).unwrap();
        db.record_verified_file("/h", "a", &file).unwrap();
        db.forget_verified_files("/g").unwrap();
        assert!(db.verified_files("/g").unwrap().is_empty());
        assert_eq!(db.verified_files("/h").unwrap().len(), 1);
    }

    #[test]
    fn env_var_names_are_unique() {
        let db = Db::open_in_memory().unwrap();
        db.add_env_var("DXVK_HUD", "fps").unwrap();
        assert!(db.add_env_var("DXVK_HUD", "full").is_err());
        let vars = db.env_vars().unwrap();
        assert_eq!(vars.len(), 1);
        db.set_env_var_enabled(vars[0].id, false).unwrap();
        assert!(!db.env_vars().unwrap()[0].enabled);
    }
}
