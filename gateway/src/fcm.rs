//! FCM HTTP v1 dispatch. Wakes carry no message content, and the service-account credential
//! never leaves the gateway.

use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use base64::Engine as _;
use common::proto::client_rel::Wake;
use common::utils::now_secs;
use jsonwebtoken::Algorithm;
use jsonwebtoken::EncodingKey;
use jsonwebtoken::Header;
use parking_lot::Mutex;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Semaphore;

const SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_IDLE_CONNS_PER_HOST: usize = 16;

/// Wakes beyond this are shed, so inbound volume cannot build an unbounded egress backlog.
const MAX_INFLIGHT_SENDS: usize = 64;

#[derive(Deserialize)]
struct ServiceAccount {
    project_id:   String,
    private_key:  String, // RSA PEM
    client_email: String,
    token_uri:    String,
}

#[derive(Serialize)]
struct JwtClaims<'a> {
    iss:   &'a str,
    scope: &'a str,
    aud:   &'a str,
    iat:   u64,
    exp:   u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in:   u64,
}

struct CachedToken {
    token:      String,
    expires_at: SystemTime,
}

pub struct FcmSender {
    http:         reqwest::Client,
    project_id:   String,
    client_email: String,
    token_uri:    String,
    encoding_key: EncodingKey,
    cached:       Mutex<Option<CachedToken>>,
    inflight:     Semaphore,
}

impl FcmSender {
    pub fn from_service_account(path: &Path) -> Result<Self> {
        let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let sa: ServiceAccount =
            serde_json::from_slice(&raw).context("parsing service-account JSON")?;
        let encoding_key = EncodingKey::from_rsa_pem(sa.private_key.as_bytes())
            .context("service-account private_key is not valid RSA PEM")?;
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .pool_max_idle_per_host(MAX_IDLE_CONNS_PER_HOST)
            .build()
            .context("building the FCM HTTP client")?;
        Ok(Self {
            http,
            project_id: sa.project_id,
            client_email: sa.client_email,
            token_uri: sa.token_uri,
            encoding_key,
            cached: Mutex::new(None),
            inflight: Semaphore::new(MAX_INFLIGHT_SENDS),
        })
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// A racing double-mint is harmless (last write wins); no lock is held across the request.
    async fn access_token(&self) -> Result<String> {
        if let Some(c) = self.cached.lock().as_ref() {
            if c.expires_at > SystemTime::now() + EXPIRY_MARGIN {
                return Ok(c.token.clone());
            }
        }

        let now = now_secs();
        let claims = JwtClaims {
            iss:   &self.client_email,
            scope: SCOPE,
            aud:   &self.token_uri,
            iat:   now,
            exp:   now + 3600,
        };
        let jwt = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.encoding_key)
            .context("signing service-account JWT")?;

        let resp = self
            .http
            .post(&self.token_uri)
            .form(&[("grant_type", JWT_BEARER), ("assertion", jwt.as_str())])
            .send()
            .await
            .context("OAuth2 token request")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("OAuth2 token request failed: {status}: {body}"));
        }
        let tok: TokenResponse = resp.json().await.context("parsing OAuth2 token response")?;

        *self.cached.lock() = Some(CachedToken {
            token:      tok.access_token.clone(),
            expires_at: SystemTime::now() + Duration::from_secs(tok.expires_in),
        });
        Ok(tok.access_token)
    }

    /// Wake data is collapsible: one pending wake drains all queued messages.
    pub async fn send(&self, device_token: &str, payload: &[u8], class: Wake) -> Result<()> {
        let _permit = self.inflight.try_acquire().map_err(|_| anyhow!("FCM dispatch saturated"))?;
        let url =
            format!("https://fcm.googleapis.com/v1/projects/{}/messages:send", self.project_id);
        self.send_to(&url, device_token, payload, class).await
    }

    async fn send_to(
        &self, url: &str, device_token: &str, payload: &[u8], class: Wake,
    ) -> Result<()> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(payload);
        // An undelivered call expires with its offer instead of ringing an hour later, and its
        // own collapse key keeps a message wake from swallowing it.
        let body = match class {
            Wake::Call => serde_json::json!({
                "message": {
                    "token": device_token,
                    "android": { "priority": "high", "collapse_key": "call", "ttl": "40s" },
                    "data": { "p": b64, "type": "call" },
                }
            }),
            _ => serde_json::json!({
                "message": {
                    "token": device_token,
                    "android": { "priority": "high", "collapse_key": "message-sync" },
                    "data": { "p": b64 },
                }
            }),
        };
        for attempt in 0..3 {
            let response: Result<_> = async {
                let access = self.access_token().await?;
                Ok(self.http.post(url).bearer_auth(access).json(&body).send().await?)
            }
            .await;
            let (error, delay) = match response {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => {
                    let status = response.status();
                    let retry_after = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    let body = response.text().await.unwrap_or_default();
                    if status == StatusCode::UNAUTHORIZED && attempt == 0 {
                        *self.cached.lock() = None;
                    }
                    let now = SystemTime::now();
                    let delay = retry_delay(Some(status), retry_after.as_deref(), attempt, now);
                    (anyhow!("FCM send failed: {status}: {body}"), delay)
                },
                Err(error) => {
                    let delay = retry_delay(None, None, attempt, SystemTime::now());
                    (anyhow!("FCM send request: {error}"), delay)
                },
            };
            let Some(delay) = delay else { return Err(error) };
            let jitter = Duration::from_millis(u64::from(rand::random::<u32>() % 250));
            tokio::time::sleep(delay + jitter).await;
        }
        unreachable!()
    }
}

/// `None` gives up; `status` is `None` when no response arrived. A 401 is retried once, after the
/// caller drops its cached access token.
fn retry_delay(
    status: Option<StatusCode>, retry_after: Option<&str>, attempt: u32, now: SystemTime,
) -> Option<Duration> {
    let base: u64 = match status {
        None => 1,
        Some(StatusCode::TOO_MANY_REQUESTS) => 60,
        Some(StatusCode::UNAUTHORIZED) if attempt == 0 => 1,
        Some(status) if status.is_server_error() => 1,
        Some(_) => return None,
    };
    let mut delay = Duration::from_secs(base << attempt);
    if let Some(header) = retry_after {
        let requested = header.parse().ok().map(Duration::from_secs).or_else(|| {
            let at = httpdate::parse_http_date(header).ok()?;
            Some(at.duration_since(now).unwrap_or_default())
        })?;
        delay = delay.max(requested);
    }
    (attempt < 2 && delay <= Duration::from_secs(5 * 60)).then_some(delay)
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use super::*;

    #[test]
    fn only_transient_failures_are_retried_and_never_past_five_minutes() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let in_90s = httpdate::fmt_http_date(now + Duration::from_secs(90));
        let passed = httpdate::fmt_http_date(now - Duration::from_secs(30));
        let cases = [
            (None, None, 0, Some(1)),
            (None, None, 1, Some(2)),
            (None, None, 2, None),
            (Some(503), None, 0, Some(1)),
            (Some(500), None, 1, Some(2)),
            (Some(503), None, 2, None),
            (Some(429), None, 0, Some(60)),
            (Some(429), None, 1, Some(120)),
            (Some(401), None, 0, Some(1)),
            (Some(401), None, 1, None),
            (Some(400), None, 0, None),
            (Some(404), Some("1"), 0, None),
            (Some(503), Some("30"), 0, Some(30)),
            (Some(503), Some("0"), 1, Some(2)),
            (Some(503), Some("300"), 0, Some(300)),
            (Some(503), Some("301"), 0, None),
            (Some(429), Some(in_90s.as_str()), 0, Some(90)),
            (Some(429), Some(passed.as_str()), 0, Some(60)),
            (Some(503), Some("soon"), 0, None),
            (Some(503), Some("30"), 2, None),
        ];
        for (status, retry_after, attempt, want) in cases {
            let status = status.map(|code| StatusCode::from_u16(code).unwrap());
            assert_eq!(
                retry_delay(status, retry_after, attempt, now),
                want.map(Duration::from_secs),
                "{status:?} Retry-After {retry_after:?} on attempt {attempt}"
            );
        }
    }
}
