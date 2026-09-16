//! S3 object reads, writes and deletes authenticated with AWS Signature V4.

use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use hmac::Hmac;
use hmac::Mac;
use hmac::digest::KeyInit;
use sha2::Digest;
use sha2::Sha256;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Generous for one 256 KiB object.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct S3 {
    http: reqwest::Client,
    /// `https://<account>.r2.cloudflarestorage.com`, no trailing slash.
    endpoint: String,
    host: String,
    bucket: String,
    region: String,
    access_key: String,
    secret_key: String,
}

impl S3 {
    pub fn new(
        endpoint: &str, bucket: &str, region: &str, access_key: &str, secret_key: &str,
    ) -> Result<Self> {
        let endpoint = endpoint.trim_end_matches('/').to_string();
        let host = endpoint
            .strip_prefix("https://")
            .or_else(|| endpoint.strip_prefix("http://"))
            .context("store endpoint must start with http:// or https://")?
            .to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building the S3 HTTP client")?;
        Ok(Self {
            http,
            endpoint,
            host,
            bucket: bucket.to_string(),
            region: region.to_string(),
            access_key: access_key.to_string(),
            secret_key: secret_key.to_string(),
        })
    }

    fn uri(&self, key: &str) -> String {
        format!("/{}/{}", self.bucket, key)
    }

    pub async fn put(&self, key: &str, body: Vec<u8>, cache_control: &str) -> Result<()> {
        let uri = self.uri(key);
        let payload_hash = hex::encode(Sha256::digest(&body));
        let (amz_date, date) = now_stamps();
        let auth = sign(
            &self.secret_key,
            &self.access_key,
            &self.region,
            "PUT",
            &self.host,
            &uri,
            &[],
            &payload_hash,
            &amz_date,
            &date,
        );
        let resp = self
            .http
            .put(format!("{}{}", self.endpoint, uri))
            .header("host", &self.host)
            .header("x-amz-date", &amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header("authorization", auth)
            .header("content-type", "application/octet-stream")
            .header("cache-control", cache_control)
            .body(body)
            .send()
            .await
            .context("S3 PUT")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "S3 PUT {key}: {status} {}",
                text.chars().take(200).collect::<String>()
            ));
        }
        Ok(())
    }

    /// Gone afterwards, whether or not it was there.
    pub async fn delete(&self, key: &str) -> Result<()> {
        let uri = self.uri(key);
        let payload_hash = hex::encode(Sha256::digest(b""));
        let (amz_date, date) = now_stamps();
        let auth = sign(
            &self.secret_key,
            &self.access_key,
            &self.region,
            "DELETE",
            &self.host,
            &uri,
            &[],
            &payload_hash,
            &amz_date,
            &date,
        );
        let resp = self
            .http
            .delete(format!("{}{}", self.endpoint, uri))
            .header("host", &self.host)
            .header("x-amz-date", &amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header("authorization", auth)
            .send()
            .await
            .context("S3 DELETE")?;
        let status = resp.status();
        if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
            return Err(anyhow!("S3 DELETE {key}: {status}"));
        }
        Ok(())
    }

    /// `None` on 404.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let uri = self.uri(key);
        let payload_hash = hex::encode(Sha256::digest(b""));
        let (amz_date, date) = now_stamps();
        let auth = sign(
            &self.secret_key,
            &self.access_key,
            &self.region,
            "GET",
            &self.host,
            &uri,
            &[],
            &payload_hash,
            &amz_date,
            &date,
        );
        let resp = self
            .http
            .get(format!("{}{}", self.endpoint, uri))
            .header("host", &self.host)
            .header("x-amz-date", &amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header("authorization", auth)
            .send()
            .await
            .context("S3 GET")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("S3 GET {key}: {status}"));
        }
        Ok(Some(resp.bytes().await.context("S3 GET body")?.to_vec()))
    }
}

/// `(YYYYMMDDTHHMMSSZ, YYYYMMDD)` for now.
fn now_stamps() -> (String, String) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil(secs);
    (format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z"), format!("{y:04}{mo:02}{d:02}"))
}

/// Unix seconds → proleptic Gregorian civil time (Howard Hinnant's algorithm).
fn civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, (rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32)
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Percent-encode a URI path the way SigV4 canonicalises it: unreserved
/// characters and `/` pass, everything else is `%XX` uppercase.
fn canonical_uri(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            },
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The `Authorization` header for one request. `extra` are further headers to
/// sign, lowercase names, beyond the three every request carries. No query
/// string: the store never sends one.
#[allow(clippy::too_many_arguments)]
fn sign(
    secret_key: &str, access_key: &str, region: &str, method: &str, host: &str, uri: &str,
    extra: &[(&str, &str)], payload_hash: &str, amz_date: &str, date: &str,
) -> String {
    let mut headers: Vec<(String, String)> = vec![
        ("host".into(), host.into()),
        ("x-amz-content-sha256".into(), payload_hash.into()),
        ("x-amz-date".into(), amz_date.into()),
    ];
    headers.extend(extra.iter().map(|(k, v)| (k.to_string(), v.trim().to_string())));
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");

    let canonical_request = format!(
        "{method}\n{}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        canonical_uri(uri)
    );
    let scope = format!("{date}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );

    let k_date = hmac(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked GET example from the S3 SigV4 reference ("Signature
    /// Calculations for the Authorization Header: Transferring Payload in a
    /// Single Chunk", GET Object). One vector pins the whole chain: URI and
    /// header canonicalisation, scope, and the four-step key derivation.
    #[test]
    fn matches_the_aws_get_object_example() {
        let empty = hex::encode(Sha256::digest(b""));
        let auth = sign(
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "AKIAIOSFODNN7EXAMPLE",
            "us-east-1",
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            &[("range", "bytes=0-9")],
            &empty,
            "20130524T000000Z",
            "20130524",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn civil_dates_are_right_at_the_edges() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(civil(1_369_353_600), (2013, 5, 24, 0, 0, 0));
        assert_eq!(civil(1_735_689_599), (2024, 12, 31, 23, 59, 59));
    }

    #[test]
    fn canonical_uri_escapes_the_reserved() {
        assert_eq!(canonical_uri("/b/packs/ab/b/cd"), "/b/packs/ab/b/cd");
        assert_eq!(canonical_uri("/b/a b+c"), "/b/a%20b%2Bc");
    }
}
