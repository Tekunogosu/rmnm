//! HTTP access to the Monsters & Memories account API and public web pages.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::{Client, Response};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const API_BASE: &str = "https://account.monstersandmemories.com/api";
/// Protocol version the official launcher sends with a login.
const LOGIN_API_VERSION: u32 = 21;

pub struct Api {
    http: Client,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwoFactorChallenge {
    pub method: String,
    pub methods: Vec<String>,
    pub challenge_token: String,
}

#[derive(Debug, Clone)]
pub enum LoginOutcome {
    Token(String),
    TwoFactor(TwoFactorChallenge),
}

/// One downloadable build, from `GET game/versions`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GameVersion {
    pub slug: String,
    pub version: String,
    pub chunks_url: String,
    pub manifest_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenStatus {
    Valid,
    /// The server refused the token; the message is its explanation.
    Rejected(String),
}

/// Every account API response carries `status` (0 is success) and, on failure, `error`/`code`.
#[derive(Debug, Deserialize)]
struct Envelope {
    status: Option<i64>,
    error: Option<String>,
    code: Option<String>,
    token: Option<String>,
    method: Option<String>,
    #[serde(default)]
    methods: Vec<String>,
    challenge_token: Option<Value>,
}

impl Envelope {
    fn failure(&self) -> anyhow::Error {
        let reason = self
            .error
            .clone()
            .or_else(|| self.code.clone())
            .unwrap_or_else(|| format!("status {}", self.status.unwrap_or(-1)));
        anyhow!(reason)
    }

    fn into_token(self) -> Result<String> {
        if self.status != Some(0) {
            return Err(self.failure());
        }
        self.token
            .filter(|t| !t.is_empty())
            .context("server sent no token")
    }
}

impl Api {
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .user_agent(concat!("rmnm/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .context("building HTTP client")?;
        Ok(Self { http })
    }

    pub fn login(&self, email: &str, password: &str) -> Result<LoginOutcome> {
        let reply: Envelope = self.post(
            "account/login",
            &json!({ "email": email, "password": password, "version": LOGIN_API_VERSION }),
        )?;
        if reply.code.as_deref() == Some("two_factor_required") {
            let challenge_token = match reply.challenge_token {
                Some(Value::String(s)) => s,
                Some(other) => other.to_string(),
                None => bail!("two-factor login started without a challenge"),
            };
            return Ok(LoginOutcome::TwoFactor(TwoFactorChallenge {
                method: reply.method.unwrap_or_else(|| "email".to_string()),
                methods: reply.methods,
                challenge_token,
            }));
        }
        reply.into_token().map(LoginOutcome::Token)
    }

    pub fn submit_two_factor(&self, challenge: &TwoFactorChallenge, code: &str) -> Result<String> {
        let reply: Envelope = self.post(
            "account/login/2fa",
            &json!({
                "method": challenge.method,
                "code": code,
                "challenge_token": challenge.challenge_token,
            }),
        )?;
        reply.into_token()
    }

    pub fn resend_two_factor(&self, challenge: &TwoFactorChallenge) -> Result<()> {
        let reply: Envelope = self.post(
            &format!("account/login/2fa/{}", challenge.method),
            &json!({ "challenge_token": challenge.challenge_token }),
        )?;
        if reply.status != Some(0) {
            return Err(reply.failure());
        }
        Ok(())
    }

    /// Asks the server whether `token` is still accepted (`GET account/me`).
    pub fn token_status(&self, token: &str) -> Result<TokenStatus> {
        let response = self
            .http
            .get(format!("{API_BASE}/account/me"))
            .header("Authorization", token)
            .send()
            .context("contacting the account server")?;
        let reply: Envelope = parse_json(response)?;
        Ok(match reply.status {
            Some(0) => TokenStatus::Valid,
            _ => TokenStatus::Rejected(reply.failure().to_string()),
        })
    }

    /// The builds this account may download (`GET game/versions`), with their URLs forced to HTTPS.
    pub fn game_versions(&self, token: &str) -> Result<Vec<GameVersion>> {
        #[derive(Deserialize)]
        struct Reply {
            versions: Option<Vec<GameVersion>>,
            error: Option<String>,
        }
        let response = self
            .http
            .get(format!("{API_BASE}/game/versions"))
            .query(&[("token", token)])
            .send()
            .context("contacting the account server")?;
        let reply: Reply = parse_json(response)?;
        let versions = reply.versions.with_context(|| {
            reply
                .error
                .unwrap_or_else(|| "server listed no game versions".into())
        })?;
        versions
            .into_iter()
            .map(|v| {
                Ok(GameVersion {
                    chunks_url: https_url(&v.chunks_url)?,
                    manifest_url: https_url(&v.manifest_url)?,
                    ..v
                })
            })
            .collect()
    }

    /// Starts a large download. Unlike API calls this allows up to 30 minutes for the body.
    pub fn download(&self, url: &str) -> Result<Response> {
        let response = self
            .http
            .get(url)
            .timeout(Duration::from_secs(30 * 60))
            .send()
            .with_context(|| format!("downloading {url}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("{url} returned HTTP {status}");
        }
        Ok(response)
    }

    /// Fetches a public page or feed as text.
    pub fn get_text(&self, url: &str) -> Result<String> {
        let response = self
            .http
            .get(url)
            .send()
            .with_context(|| format!("fetching {url}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("{url} returned HTTP {status}");
        }
        response.text().with_context(|| format!("reading {url}"))
    }

    fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        let response = self
            .http
            .post(format!("{API_BASE}/{path}"))
            .json(body)
            .send()
            .context("contacting the account server")?;
        parse_json(response)
    }
}

/// The server hands out `http://` URLs for the manifest and chunks; the same hosts serve HTTPS
/// (plain HTTP only redirects there), so upgrade rather than fetch the manifest unprotected.
fn https_url(url: &str) -> Result<String> {
    if let Some(rest) = url.strip_prefix("http://") {
        Ok(format!("https://{rest}"))
    } else if url.starts_with("https://") {
        Ok(url.to_string())
    } else {
        bail!("refusing non-HTTP download URL {url:?}")
    }
}

fn parse_json<T: DeserializeOwned>(response: Response) -> Result<T> {
    let status = response.status();
    let body = response.bytes().context("reading the server's reply")?;
    serde_json::from_slice(&body).with_context(|| format!("unexpected reply (HTTP {status})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(body: &str) -> Envelope {
        serde_json::from_str(body).unwrap()
    }

    #[test]
    fn rejected_session_reports_server_message() {
        let reply = envelope(
            r#"{"error":"Session expired or was revoked. Please sign in again.","code":"reauthentication_required","status":1}"#,
        );
        assert_eq!(
            reply.into_token().unwrap_err().to_string(),
            "Session expired or was revoked. Please sign in again."
        );
    }

    #[test]
    fn download_urls_are_forced_to_https() {
        assert_eq!(
            https_url("http://clients.monstersandmemories.com/chunks").unwrap(),
            "https://clients.monstersandmemories.com/chunks"
        );
        assert_eq!(https_url("https://a/b").unwrap(), "https://a/b");
        assert!(https_url("file:///etc").is_err());
    }

    #[test]
    fn success_requires_a_token() {
        assert_eq!(
            envelope(r#"{"status":0,"token":"a.b.c"}"#)
                .into_token()
                .unwrap(),
            "a.b.c"
        );
        assert!(envelope(r#"{"status":0}"#).into_token().is_err());
        assert!(
            envelope(r#"{"status":3,"code":"bad_password"}"#)
                .into_token()
                .is_err()
        );
    }
}
