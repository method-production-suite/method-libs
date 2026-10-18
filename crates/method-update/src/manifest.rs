//! `latest.json`, Tauri's update manifest format, read the way
//! tauri-plugin-updater 2.12.0 reads it, so this crate accepts exactly what
//! the suite's release pipelines already publish: `version` (or its alias
//! `name`, a leading `v` dropped, semver), optional `notes` and an RFC 3339
//! `pub_date`, and either a `platforms` map (the static format, which every
//! suite app publishes) or a top-level `url` and `signature` (the dynamic
//! format, which none does, read the same way anyway).

use std::collections::BTreeMap;
use std::str::FromStr;

use semver::Version;
use serde::{Deserialize, Serialize};

/// One platform's file: where to download it and the CONTENT of its `.sig`
/// (base64 of the minisign signature box).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub url: String,
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Data {
    /// `platforms: { "darwin-aarch64": { url, signature }, ... }`
    Static(BTreeMap<String, Platform>),
    /// `url` and `signature` at the top, for every platform alike.
    Dynamic(Platform),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub notes: Option<String>,
    pub pub_date: Option<String>,
    pub data: Data,
}

impl Release {
    /// `RemoteRelease::download_url` and `signature` together: the entry
    /// for one platform key, or the plugin's own words for its absence.
    pub fn platform(&self, key: &str) -> Result<&Platform, String> {
        match &self.data {
            Data::Dynamic(p) => Ok(p),
            Data::Static(map) => map.get(key).ok_or_else(|| {
                format!("the platform `{key}` was not found in the response `platforms` object")
            }),
        }
    }

    /// Parse a manifest, with the plugin's rules and its words for what is
    /// wrong.
    pub fn parse(text: &str) -> Result<Release, String> {
        let raw: Raw = serde_json::from_str(text).map_err(|e| e.to_string())?;
        raw.into_release()
    }

    pub fn from_value(value: serde_json::Value) -> Result<Release, String> {
        let raw: Raw = serde_json::from_value(value).map_err(|e| e.to_string())?;
        raw.into_release()
    }
}

#[derive(Deserialize)]
struct Raw {
    #[serde(alias = "name")]
    version: String,
    notes: Option<String>,
    pub_date: Option<String>,
    platforms: Option<BTreeMap<String, Platform>>,
    url: Option<String>,
    signature: Option<String>,
}

impl Raw {
    fn into_release(self) -> Result<Release, String> {
        let version = parse_version(&self.version)?;
        if let Some(date) = &self.pub_date
            && !is_rfc3339(date)
        {
            return Err(format!(
                "invalid value for `pub_date`: {date:?} is not an RFC 3339 date"
            ));
        }
        let data = match self.platforms {
            Some(platforms) => Data::Static(platforms),
            None => Data::Dynamic(Platform {
                url: self
                    .url
                    .ok_or("the `url` field was not set on the updater response")?,
                signature: self
                    .signature
                    .ok_or("the `signature` field was not set on the updater response")?,
            }),
        };
        Ok(Release {
            version,
            notes: self.notes,
            pub_date: self.pub_date,
            data,
        })
    }
}

/// `parse_version` in the plugin: every leading `v` trimmed, then semver.
pub fn parse_version(text: &str) -> Result<Version, String> {
    Version::from_str(text.trim_start_matches('v')).map_err(|e| e.to_string())
}

/// RFC 3339 date-time, as the plugin's `time` parser requires of
/// `pub_date`: `YYYY-MM-DDTHH:MM:SS[.frac](Z|+HH:MM|-HH:MM)`.
pub fn is_rfc3339(s: &str) -> bool {
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| {
        b.get(r.clone())
            .is_some_and(|d| d.len() == r.len() && d.iter().all(u8::is_ascii_digit))
    };
    let num = |r: std::ops::Range<usize>| -> u32 { s[r].parse().unwrap_or(u32::MAX) };
    if b.len() < 20
        || !digits(0..4)
        || b[4] != b'-'
        || !digits(5..7)
        || b[7] != b'-'
        || !digits(8..10)
        || !matches!(b[10], b'T' | b't')
        || !digits(11..13)
        || b[13] != b':'
        || !digits(14..16)
        || b[16] != b':'
        || !digits(17..19)
    {
        return false;
    }
    if !(1..=12).contains(&num(5..7))
        || !(1..=31).contains(&num(8..10))
        || num(11..13) > 23
        || num(14..16) > 59
        || num(17..19) > 60
    {
        return false;
    }
    let mut i = 19;
    if b[i] == b'.' {
        let start = i + 1;
        i = start;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    match &b[i..] {
        [b'Z' | b'z'] => true,
        [b'+' | b'-', ..] if b.len() - i == 6 => {
            digits(i + 1..i + 3)
                && b[i + 3] == b':'
                && digits(i + 4..i + 6)
                && num(i + 1..i + 3) <= 23
                && num(i + 4..i + 6) <= 59
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIPPED: &str = r#"{
  "version": "0.11.1",
  "notes": "Method Example 0.11.1. Release notes: https://github.com/example/app/releases/tag/v0.11.1",
  "pub_date": "2026-10-10T18:00:00.000Z",
  "platforms": {
    "darwin-aarch64": {
      "url": "https://github.com/example/app/releases/download/v0.11.1/Method-Example-macOS.app.tar.gz",
      "signature": "c2ln"
    }
  }
}"#;

    #[test]
    fn reads_the_manifest_the_release_pipelines_publish() {
        let r = Release::parse(SHIPPED).unwrap();
        assert_eq!(r.version, Version::new(0, 11, 1));
        assert!(r.platform("darwin-aarch64").is_ok());
        assert_eq!(
            r.platform("darwin-x86_64").unwrap_err(),
            "the platform `darwin-x86_64` was not found in the response `platforms` object"
        );
    }

    #[test]
    fn name_is_an_alias_and_a_leading_v_is_dropped() {
        let r =
            Release::parse(r#"{"name":"vv1.2.3","url":"https://x/y","signature":"s"}"#).unwrap();
        assert_eq!(r.version, Version::new(1, 2, 3));
        assert!(matches!(r.data, Data::Dynamic(_)));
        assert_eq!(r.platform("anything").unwrap().url, "https://x/y");
    }

    #[test]
    fn a_dynamic_manifest_needs_both_url_and_signature() {
        assert_eq!(
            Release::parse(r#"{"version":"1.0.0","signature":"s"}"#).unwrap_err(),
            "the `url` field was not set on the updater response"
        );
        assert_eq!(
            Release::parse(r#"{"version":"1.0.0","url":"https://x"}"#).unwrap_err(),
            "the `signature` field was not set on the updater response"
        );
    }

    #[test]
    fn a_bad_version_or_date_is_refused() {
        assert!(Release::parse(r#"{"version":"main","url":"u","signature":"s"}"#).is_err());
        assert!(
            Release::parse(
                r#"{"version":"1.0.0","pub_date":"yesterday","url":"u","signature":"s"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn rfc3339_as_the_plugin_reads_it() {
        for ok in [
            "2026-10-10T18:00:00Z",
            "2026-10-10T18:00:00.123Z",
            "2026-10-10T18:00:00+02:00",
            "2026-10-10t18:00:00z",
        ] {
            assert!(is_rfc3339(ok), "{ok}");
        }
        for bad in [
            "2026-10-10",
            "2026-10-10 18:00:00Z",
            "2026-13-10T18:00:00Z",
            "2026-10-10T18:00:00",
            "2026-10-10T18:00:00.Z",
            "2026-10-10T18:00:00+0200",
        ] {
            assert!(!is_rfc3339(bad), "{bad}");
        }
    }
}
