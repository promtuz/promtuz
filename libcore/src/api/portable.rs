//! Rules that are the product's, not the platform's.
//!
//! Each of these was written in Kotlin and would have been written again in
//! Swift: a link format, a set of date buckets, the shape of an update
//! manifest. None of them are decisions Android or iOS get to make differently,
//! so they belong on the shared side of the FFI.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::platform::CoreError;

fn bad(msg: impl Into<String>) -> CoreError {
    CoreError::Internal { msg: msg.into() }
}

// ── Invite links ──────────────────────────────────────────────────────────

/// The shareable form of an invite: `https://promtuz.dev/pair#<base64url>`.
///
/// A URL contract, so both ends of it live together — a client that built the
/// link one way and parsed it another would fail only at the moment someone
/// tried to pair.
const PAIR_PREFIX: &str = "https://promtuz.dev/pair#";

#[uniffi::export]
pub fn invite_link(invite: Vec<u8>) -> String {
    format!("{PAIR_PREFIX}{}", URL_SAFE_NO_PAD.encode(invite))
}

/// The invite bytes inside a pair link, or `None` if it isn't one.
///
/// Accepts the code in the fragment or in `?i=`, since a link that has been
/// through a chat app, a QR reader and a browser does not always keep its
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

// ── Timestamp buckets ─────────────────────────────────────────────────────

/// How far in the past a timestamp is, in the terms a chat list thinks in.
///
/// The *bucketing* is a product decision and lives here. The *formatting* does
/// not: a platform date formatter honours the reader's locale, calendar and
/// 24-hour preference, none of which this should try to reimplement. So each
/// client turns a bucket into text its own way.
#[derive(uniffi::Enum, Debug, PartialEq, Eq)]
pub enum TimeBucket {
    /// Same calendar day — show a clock time.
    Today,
    /// The calendar day before — show the word.
    Yesterday,
    /// Within the last week — show a weekday name.
    ThisWeek,
    /// Same calendar year — show day and month.
    ThisYear,
    /// Anything older — show the full date.
    Older,
}

/// Which bucket `ts_ms` falls into, given the reader's `now_ms` and their
/// UTC offset in seconds — the caller supplies both, because a core that asked
/// the system clock could not be tested and a core that assumed UTC would flip
/// "Yesterday" at the wrong hour for most of the world.
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

// ── Update manifests ──────────────────────────────────────────────────────

/// A build the update server is offering.
#[derive(uniffi::Record)]
pub struct UpdateManifest {
    pub version_code: u32,
    pub version_name: String,
    pub apk: String,
    pub size: u64,
    pub sha256: String,
}

/// Check a manifest against the contract before anything is downloaded.
///
/// Every field here is attacker-influenced — it arrives over the network — so
/// the filename is required to match the version it claims, rather than being
/// trusted to name a file we then fetch.
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

/// Whether an offered build may be installed over `installed_code`.
///
/// Equal version codes pass when the channel changes: the two binaries are
/// genuinely different builds that happen to share a number.
#[uniffi::export]
pub fn update_is_installable(
    offered_code: u32, installed_code: u64, switching_channel: bool,
) -> bool {
    let min = if switching_channel { installed_code } else { installed_code + 1 };
    offered_code as u64 >= min
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: u64 = 86_400_000;
    const NOW: u64 = 1_700_000_000_000;

    #[test]
    fn buckets_walk_backwards_from_today() {
        assert_eq!(time_bucket(NOW, NOW, 0), TimeBucket::Today);
        assert_eq!(time_bucket(NOW - DAY_MS, NOW, 0), TimeBucket::Yesterday);
        assert_eq!(time_bucket(NOW - 3 * DAY_MS, NOW, 0), TimeBucket::ThisWeek);
        assert_eq!(time_bucket(NOW - 30 * DAY_MS, NOW, 0), TimeBucket::ThisYear);
        assert_eq!(time_bucket(NOW - 400 * DAY_MS, NOW, 0), TimeBucket::Older);
    }

    /// The offset is the whole reason this takes one: a message sent at 23:30
    /// local is still "Today" to its reader, whatever UTC thinks.
    #[test]
    fn the_day_boundary_follows_the_reader() {
        // 00:30 UTC, which is 19:30 the previous day at UTC-5.
        let utc_midnight_plus_30m = 1_700_006_400_000 + 1_800_000;
        let half_hour_before = utc_midnight_plus_30m - 3_600_000;
        assert_eq!(
            time_bucket(half_hour_before, utc_midnight_plus_30m, 0),
            TimeBucket::Yesterday,
            "in UTC the two straddle midnight"
        );
        assert_eq!(
            time_bucket(half_hour_before, utc_midnight_plus_30m, -5 * 3600),
            TimeBucket::Today,
            "at UTC-5 it is still the same evening"
        );
    }

    #[test]
    fn an_invite_link_round_trips() {
        let invite = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF];
        let link = invite_link(invite.clone());
        assert!(link.starts_with(PAIR_PREFIX));
        assert_eq!(invite_from_link(link), Some(invite.clone()));
        assert_eq!(
            invite_from_link(format!("https://promtuz.dev/pair?i={}", URL_SAFE_NO_PAD.encode(&invite))),
            Some(invite),
        );
        assert_eq!(invite_from_link("https://promtuz.dev/pair".into()), None);
        assert_eq!(invite_from_link("https://promtuz.dev/pair#not base64".into()), None);
    }

    fn manifest() -> UpdateManifest {
        UpdateManifest {
            version_code: 16,
            version_name: "0.3.5".into(),
            apk:          "promtuz-0.3.5~16.apk".into(),
            size:         1024,
            sha256:       "a".repeat(64),
        }
    }

    #[test]
    fn a_manifest_must_name_the_file_it_claims() {
        assert!(validate_update_manifest(&manifest()).is_ok());

        // The filename is the attacker's lever: it names what gets fetched.
        let mut m = manifest();
        m.apk = "../../etc/passwd".into();
        assert!(validate_update_manifest(&m).is_err());

        let mut m = manifest();
        m.version_name = "0.3.5/../".into();
        assert!(validate_update_manifest(&m).is_err());

        let mut m = manifest();
        m.sha256 = "A".repeat(64);
        assert!(validate_update_manifest(&m).is_err(), "digests are lowercase hex");
    }

    #[test]
    fn same_version_installs_only_when_the_channel_changes() {
        assert!(!update_is_installable(16, 16, false));
        assert!(update_is_installable(16, 16, true));
        assert!(update_is_installable(17, 16, false));
        assert!(!update_is_installable(15, 16, true));
    }

}
