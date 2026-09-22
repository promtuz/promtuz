//! FCM HTTP v1 dispatch. Holds the service-account credential (which never
//! leaves the gateway), caches an OAuth2 access token, and posts contentless
//! wake messages. Message content stays at the relay.

use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use base64::Engine as _;
use common::proto::client_rel::Wake;
use ed25519_dalek::ed25519::signature::rand_core::OsRng;
use ed25519_dalek::ed25519::signature::rand_core::RngCore;
use jsonwebtoken::Algorithm;
use jsonwebtoken::EncodingKey;
use jsonwebtoken::Header;
use parking_lot::Mutex;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Semaphore;

const SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// Re-mint the access token this far before its stated expiry, to cover clock
/// skew and request latency.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_IDLE_CONNS_PER_HOST: usize = 16;

/// Ceiling on outbound requests in flight. A wake beyond it is shed, so
/// inbound wake volume cannot grow an unbounded egress backlog.
const MAX_INFLIGHT_SENDS: usize = 64;

/// The fields we need out of a Google service-account JSON.
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

/// Sends FCM HTTP v1 messages under a service-account credential.
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

    /// A valid access token, minting a fresh one when the cache is empty or
    /// within [`EXPIRY_MARGIN`] of expiry. A racing double-mint is harmless
    /// (last write wins); no lock is held across the network call.
    async fn access_token(&self) -> Result<String> {
        if let Some(c) = self.cached.lock().as_ref() {
            if c.expires_at > SystemTime::now() + EXPIRY_MARGIN {
                return Ok(c.token.clone());
            }
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| anyhow!("system clock before epoch"))?
            .as_secs();
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
        // A call that cannot be delivered while it rings is not worth
        // delivering: FCM drops it after the offer's life instead of ringing
        // a phone that comes back an hour later. Its own collapse key keeps a
        // message wake from swallowing it.
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
                    let error = anyhow!("FCM send failed: {status}: {body}");
                    if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                        *self.cached.lock() = None;
                    } else if !status.is_server_error()
                        && status != reqwest::StatusCode::TOO_MANY_REQUESTS
                    {
                        return Err(error);
                    }
                    let minimum =
                        if status == reqwest::StatusCode::TOO_MANY_REQUESTS { 60 } else { 1 };
                    let mut delay = Duration::from_secs(minimum * (1 << attempt));
                    if let Some(header) = retry_after {
                        let requested = header.parse::<u64>().ok().map(Duration::from_secs)
                            .or_else(|| httpdate::parse_http_date(&header).ok().map(|time| {
                                time.duration_since(SystemTime::now()).unwrap_or_default()
                            }));
                        let Some(requested) = requested else { return Err(error) };
                        delay = delay.max(requested);
                    }
                    (error, delay)
                },
                Err(error) => {
                    (anyhow!("FCM send request: {error}"), Duration::from_secs(1 << attempt))
                },
            };
            if attempt == 2 || delay > Duration::from_secs(5 * 60) {
                return Err(error);
            }
            let jitter = Duration::from_millis(u64::from(OsRng.next_u32() % 250));
            tokio::time::sleep(delay + jitter).await;
        }
        unreachable!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retries_transient_dispatch_failure() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/send", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for status in ["503 Service Unavailable", "200 OK"] {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).await.unwrap();
                let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(json["message"]["android"]["collapse_key"], "message-sync");
                reader.get_mut().write_all(format!(
                    "HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",
                ).as_bytes()).await.unwrap();
            }
        });
        let sender = FcmSender {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            project_id: "test".into(),
            client_email: String::new(),
            token_uri: String::new(),
            encoding_key: EncodingKey::from_secret(b"unused: cached access token"),
            cached: Mutex::new(Some(CachedToken {
                token: "test".into(),
                expires_at: SystemTime::now() + Duration::from_secs(3600),
            })),
            inflight: Semaphore::new(1),
        };
        tokio::time::timeout(Duration::from_secs(5), sender.send_to(&url, "device", &[], Wake::Message))
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
    }

    /// A call wake must reach the phone as a call: its own collapse key, a
    /// life no longer than the ring, and a type the app switches on.
    #[tokio::test]
    async fn call_wake_is_short_lived_and_typed() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/send", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["message"]["android"]["collapse_key"], "call");
            assert_eq!(json["message"]["android"]["ttl"], "40s");
            assert_eq!(json["message"]["data"]["type"], "call");
            reader.get_mut().write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            ).await.unwrap();
        });
        let sender = FcmSender {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            project_id: "test".into(),
            client_email: String::new(),
            token_uri: String::new(),
            encoding_key: EncodingKey::from_secret(b"unused: cached access token"),
            cached: Mutex::new(Some(CachedToken {
                token: "test".into(),
                expires_at: SystemTime::now() + Duration::from_secs(3600),
            })),
            inflight: Semaphore::new(1),
        };
        tokio::time::timeout(Duration::from_secs(5), sender.send_to(&url, "device", &[], Wake::Call))
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
    }

    #[test]
    fn rejects_non_json() {
        let dir = std::env::temp_dir().join("pz_fcm_bad.json");
        std::fs::write(&dir, b"not json").unwrap();
        assert!(FcmSender::from_service_account(&dir).is_err());
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn rejects_bad_private_key() {
        let dir = std::env::temp_dir().join("pz_fcm_badkey.json");
        let sa = serde_json::json!({
            "project_id": "p",
            "private_key": "-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----\n",
            "client_email": "x@y.iam.gserviceaccount.com",
            "token_uri": "https://oauth2.googleapis.com/token",
        });
        std::fs::write(&dir, serde_json::to_vec(&sa).unwrap()).unwrap();
        assert!(FcmSender::from_service_account(&dir).is_err());
        let _ = std::fs::remove_file(&dir);
    }
}
