//! FCM HTTP v1 dispatch. Holds the service-account credential (which never
//! leaves the gateway), mints + caches an OAuth2 access token, and posts wake
//! messages. The gateway never inspects the payload — it is opaque ciphertext.

use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use base64::Engine as _;
use jsonwebtoken::Algorithm;
use jsonwebtoken::EncodingKey;
use jsonwebtoken::Header;
use parking_lot::Mutex;
use serde::Deserialize;
use serde::Serialize;

const SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// Re-mint the access token this far before its stated expiry, to cover clock
/// skew and request latency.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

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
}

impl FcmSender {
    pub fn from_service_account(path: &Path) -> Result<Self> {
        let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let sa: ServiceAccount =
            serde_json::from_slice(&raw).context("parsing service-account JSON")?;
        let encoding_key = EncodingKey::from_rsa_pem(sa.private_key.as_bytes())
            .context("service-account private_key is not valid RSA PEM")?;
        Ok(Self {
            http: reqwest::Client::new(),
            project_id: sa.project_id,
            client_email: sa.client_email,
            token_uri: sa.token_uri,
            encoding_key,
            cached: Mutex::new(None),
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

    /// Wake a device: a high-priority data message carrying the opaque payload
    /// base64'd into one data field, which the device's FCM handler decrypts.
    pub async fn send(&self, device_token: &str, payload: &[u8]) -> Result<()> {
        let access = self.access_token().await?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(payload);
        let body = serde_json::json!({
            "message": {
                "token": device_token,
                "android": { "priority": "high" },
                "data": { "p": b64 },
            }
        });
        let url =
            format!("https://fcm.googleapis.com/v1/projects/{}/messages:send", self.project_id);
        let resp = self
            .http
            .post(url)
            .bearer_auth(access)
            .json(&body)
            .send()
            .await
            .context("FCM send request")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("FCM send failed: {status}: {body}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
