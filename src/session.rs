//! Session tokens: reading their claims and importing one from the official launcher.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    pub email: Option<String>,
    /// Unix seconds.
    pub expires_at: i64,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Reads the payload of a JWT without verifying its signature; the server remains the authority
/// on validity, this only tells us when the token will stop working.
pub fn decode_claims(token: &str) -> Result<Claims> {
    #[derive(Deserialize)]
    struct Payload {
        email: Option<String>,
        exp: i64,
    }
    let payload = token.split('.').nth(1).context("token is not a JWT")?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .context("token payload is not base64url")?;
    let payload: Payload = serde_json::from_slice(&bytes).context("token payload is not JSON")?;
    Ok(Claims {
        email: payload.email,
        expires_at: payload.exp,
    })
}

#[derive(Clone, PartialEq, Eq)]
pub struct OfficialToken {
    pub token: String,
    pub username: Option<String>,
}

impl std::fmt::Debug for OfficialToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfficialToken")
            .field("token", &"<redacted>")
            .field("username", &self.username)
            .finish()
    }
}

/// Reads the saved token from the official launcher's `launcher.db`, opened read-only.
/// Returns `None` when the file does not exist or holds no token.
pub fn read_official_token(path: &Path) -> Result<Option<OfficialToken>> {
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", path.display()))?;
    let read = |variable: &str| -> Result<Option<String>> {
        Ok(conn
            .query_row(
                "SELECT value FROM settings WHERE variable = ?1",
                [variable],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .with_context(|| format!("reading {}", path.display()))?
            .flatten()
            .filter(|v| !v.is_empty()))
    };
    let Some(token) = read("token")? else {
        return Ok(None);
    };
    Ok(Some(OfficialToken {
        token,
        username: read("username")?,
    }))
}

/// Formats Unix seconds as a UTC `YYYY-MM-DD` date.
pub fn format_date(unix: i64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = unix.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Rejects anything that is not a plausible token before it is stored or sent.
pub fn ensure_token_shape(token: &str) -> Result<()> {
    if token.split('.').count() != 3 || token.chars().any(char::is_whitespace) {
        bail!("not a session token");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(payload: &str) -> String {
        format!(
            "eyJhbGciOiJIUzI1NiJ9.{}.c2ln",
            URL_SAFE_NO_PAD.encode(payload)
        )
    }

    #[test]
    fn decodes_exp_and_email() {
        let claims =
            decode_claims(&jwt(r#"{"email":"a@b.c","exp":1793279610,"aud":"mnm"}"#)).unwrap();
        assert_eq!(claims.email.as_deref(), Some("a@b.c"));
        assert_eq!(claims.expires_at, 1_793_279_610);
    }

    #[test]
    fn rejects_non_jwt() {
        assert!(decode_claims("nope").is_err());
        assert!(ensure_token_shape("a.b").is_err());
        assert!(ensure_token_shape(&jwt(r#"{"exp":1}"#)).is_ok());
    }

    #[test]
    fn formats_dates() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(951_782_400), "2000-02-29");
        assert_eq!(format_date(1_793_279_610), "2026-10-29");
    }

    #[test]
    fn reads_official_launcher_db_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("launcher.db");
        let conn = Connection::open(&path).unwrap();
        // The official launcher's schema, as created by its `load_database` command.
        conn.execute_batch(
            "CREATE TABLE settings (variable text primary key, value text);
             CREATE TABLE game_versions (slug text primary key, version text);
             INSERT INTO settings VALUES ('current_game','mnm'),('username','a@b.c'),
                                         ('remember','true'),('token','x.y.z');",
        )
        .unwrap();
        drop(conn);
        let found = read_official_token(&path).unwrap().unwrap();
        assert_eq!(found.token, "x.y.z");
        assert_eq!(found.username.as_deref(), Some("a@b.c"));

        assert_eq!(
            read_official_token(&dir.path().join("missing.db")).unwrap(),
            None
        );
    }

    #[test]
    fn logged_out_official_db_has_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("launcher.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE settings (variable text primary key, value text);
             INSERT INTO settings VALUES ('token','');",
        )
        .unwrap();
        drop(conn);
        assert_eq!(read_official_token(&path).unwrap(), None);
    }
}
