//! Product rules shared by every platform: the invite link format, date buckets, update manifests.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::platform::CoreError;

fn bad(msg: impl Into<String>) -> CoreError {
    CoreError::Internal { msg: msg.into() }
}

/// Building and parsing the link live together so the two cannot drift.
const PAIR_PREFIX: &str = "https://promtuz.dev/pair#";

#[uniffi::export]
pub fn invite_link(invite: Vec<u8>) -> String {
    format!("{PAIR_PREFIX}{}", URL_SAFE_NO_PAD.encode(invite))
}

/// Also accepts `?i=`, since a link passed through chat apps, QR readers and browsers can lose its
/// fragment.
#[uniffi::export]
pub fn invite_from_link(url: String) -> Option<Vec<u8>> {
    let code = url
        .split_once('#')
        .map(|(_, frag)| frag)
        .filter(|f| !f.is_empty())
        .or_else(|| {
            url.split_once("?i=").or_else(|| url.split_once("&i="))
                .map(|(_, rest)| rest.split(['&', '#']).next().unwrap_or(""))
                .filter(|c| !c.is_empty())
        })?;
    URL_SAFE_NO_PAD.decode(code).ok()
}

/// Bucketing is shared; each platform formats a bucket with its own locale-aware formatter.
#[derive(uniffi::Enum, Debug, PartialEq, Eq)]
pub enum TimeBucket {
    /// Same local day: show a clock time.
    Today,
    /// The day before: show the word.
    Yesterday,
    /// Two to six days ago: show a weekday name.
    ThisWeek,
    /// Under 365 days ago: show day and month.
    ThisYear,
    /// Show the full date.
    Older,
}

/// The caller supplies `now_ms` and its UTC offset so day boundaries fall at the reader's midnight.
#[uniffi::export]
pub fn time_bucket(ts_ms: u64, now_ms: u64, utc_offset_secs: i32) -> TimeBucket {
    const DAY: i64 = 86_400;
    let local_day = |ms: u64| (ms as i64 / 1000 + utc_offset_secs as i64).div_euclid(DAY);
    let (then, today) = (local_day(ts_ms), local_day(now_ms));
    match today - then {
        d if d <= 0 => TimeBucket::Today,
        1 => TimeBucket::Yesterday,
        2..=6 => TimeBucket::ThisWeek,
        // Not a calendar-year comparison, deliberately: 365 days back is the
        // same "long ago" to a reader, and it needs no calendar to compute.
        d if d < 365 => TimeBucket::ThisYear,
        _ => TimeBucket::Older,
    }
}

#[derive(uniffi::Record)]
pub struct UpdateManifest {
    pub version_code: u32,
    pub version_name: String,
    pub apk: String,
    pub size: u64,
    pub sha256: String,
}

/// Every field arrives over the network, so the filename must match the version it claims rather
/// than being trusted to name the file we fetch.
#[uniffi::export]
pub fn validate_update_manifest(m: &UpdateManifest) -> Result<(), CoreError> {
    if m.version_code == 0 || m.size == 0 {
        return Err(bad("Update manifest contains invalid version or size."));
    }
    let name_ok = !m.version_name.is_empty()
        && m.version_name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && m.version_name.chars().all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c));
    if !name_ok {
        return Err(bad("Update manifest contains invalid version name."));
    }
    if m.apk != format!("promtuz-{}~{}.apk", m.version_name, m.version_code) {
        return Err(bad("Update filename is invalid."));
    }
    if m.sha256.len() != 64 || !m.sha256.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
    {
        return Err(bad("Update manifest contains invalid hash."));
    }
    Ok(())
}

/// Equal version codes pass when switching channel: they are different builds sharing a number.
#[uniffi::export]
pub fn update_is_installable(
    offered_code: u32, installed_code: u64, switching_channel: bool,
) -> bool {
    let min = if switching_channel { installed_code } else { installed_code + 1 };
    offered_code as u64 >= min
}

/// `major.minor` of a `major.minor.patch[-pre]` name; a missing part counts as 0.
fn major_minor(name: &str) -> (u64, u64) {
    let core = name.split('-').next().unwrap_or("");
    let mut parts = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

/// A major or minor step cannot be skipped: the offered build is the only one that keeps working.
#[uniffi::export]
pub fn update_is_required(installed: String, offered: String) -> bool {
    major_minor(&installed) != major_minor(&offered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_must_name_the_file_it_claims() {
        let manifest = |name: &str, apk: &str, sha256: &str| UpdateManifest {
            version_code: 16,
            version_name: name.into(),
            apk:          apk.into(),
            size:         1024,
            sha256:       sha256.into(),
        };
        let (digest, upper, short) = ("a".repeat(64), "A".repeat(64), "a".repeat(63));
        assert!(
            validate_update_manifest(&manifest("0.3.5", "promtuz-0.3.5~16.apk", &digest)).is_ok()
        );
        for (name, apk, sha256) in [
            ("0.3.5", "../../etc/passwd", digest.as_str()),
            ("0.3.5", "promtuz-0.3.5~17.apk", digest.as_str()),
            ("0.3.5/../", "promtuz-0.3.5/../~16.apk", digest.as_str()),
            ("0.3.5", "promtuz-0.3.5~16.apk", upper.as_str()),
            ("0.3.5", "promtuz-0.3.5~16.apk", short.as_str()),
        ] {
            assert!(
                validate_update_manifest(&manifest(name, apk, sha256)).is_err(),
                "{name} {apk} {sha256}"
            );
        }

        // Only a major or minor step forces the update, never a patch or a pre-release suffix.
        for (installed, offered, required) in [
            ("0.4.3-beta1", "0.5.0", true),
            ("0.4.3", "1.0.0", true),
            ("0.10.0", "0.9.9", true),
            ("0.4.2", "0.4.3", false),
            ("0.4.3-beta1", "0.4.3", false),
        ] {
            assert_eq!(
                update_is_required(installed.into(), offered.into()),
                required,
                "{installed} -> {offered}"
            );
        }
    }
}
