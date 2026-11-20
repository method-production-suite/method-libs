//! Rollback and freeze protection: the highest version an update check
//! ever offered and this app accepted, kept per app, and the rule that an
//! offer lower than it is refused.
//!
//! The check already refuses anything not newer than the running version,
//! and the signature binds the announced version. This adds memory across
//! runs: once a signed 1.4.0 was offered and downloaded, a server (or
//! anyone in front of it) that later offers an older signed release, say
//! 1.3.0 to a copy still on 1.2.0 because it never restarted, is refused.
//! The same offer again (1.4.0) and anything newer still pass.
//!
//! A version is remembered only after its download passed its signature
//! check, so a tampered manifest announcing a huge version can not freeze
//! updates. The mark lives in a small JSON file the app's settings file
//! sits beside ([`HighWater::beside`]), written atomically. A missing,
//! unreadable or damaged file counts as no mark: never a failure, never a
//! panic, and the next accepted offer writes a good one.

use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};

/// An offer lower than the mark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rollback {
    pub offered: Version,
    pub highest: Version,
}

impl std::fmt::Display for Rollback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The update server offered version {}, older than version {} it offered before, so it was refused.",
            self.offered, self.highest
        )
    }
}

impl std::error::Error for Rollback {}

#[derive(Serialize, Deserialize)]
struct Stored {
    highest: String,
}

/// The mark for one app: in a file, or in memory only.
#[derive(Clone, Debug, Default)]
pub struct HighWater {
    file: Option<PathBuf>,
    /// What this run accepted, for a mark kept in memory (or a file that
    /// could not be written).
    seen: Option<Version>,
}

impl HighWater {
    /// A mark kept in `file`.
    pub fn at(file: PathBuf) -> Self {
        Self {
            file: Some(file),
            seen: None,
        }
    }

    /// A mark kept in memory only, for this run.
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// The file for the mark beside an app's updater settings file:
    /// `updates.json` gives `updates.high-water.json`.
    pub fn beside(settings_file: &Path) -> PathBuf {
        let stem = settings_file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "updates".into());
        settings_file.with_file_name(format!("{stem}.high-water.json"))
    }

    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// The mark in `file`: `None` when it is missing or can't be read.
    pub fn read(file: &Path) -> Option<Version> {
        let text = std::fs::read_to_string(file).ok()?;
        let stored: Stored = serde_json::from_str(&text).ok()?;
        crate::manifest::parse_version(&stored.highest).ok()
    }

    /// The highest version accepted so far, from the file and this run.
    pub fn highest(&self) -> Option<Version> {
        let stored = self.file.as_deref().and_then(Self::read);
        match (stored, self.seen.clone()) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    /// Refuses an offer lower than the mark. Equal passes: the same release
    /// offered again (downloaded, not yet installed) is not a rollback.
    pub fn admit(&self, offered: &Version) -> Result<(), Rollback> {
        match self.highest() {
            Some(highest) if *offered < highest => Err(Rollback {
                offered: offered.clone(),
                highest,
            }),
            _ => Ok(()),
        }
    }

    /// Remembers `accepted` when it is above the mark. Call it once the
    /// offer's download passed its signature check. A file that can't be
    /// written is reported; the mark still holds for this run.
    pub fn record(&mut self, accepted: &Version) -> std::io::Result<()> {
        if self.highest().is_some_and(|h| h >= *accepted) {
            return Ok(());
        }
        self.seen = Some(accepted.clone());
        match &self.file {
            Some(f) => {
                let bytes = serde_json::to_vec_pretty(&Stored {
                    highest: accepted.to_string(),
                })
                .map_err(std::io::Error::other)?;
                crate::fsutil::write_atomically(f, &bytes)
            }
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn an_offer_lower_than_the_mark_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("updates.high-water.json");
        let mut hw = HighWater::at(file.clone());
        hw.record(&v("1.4.0")).unwrap();
        assert_eq!(
            hw.admit(&v("1.3.0")).unwrap_err(),
            Rollback {
                offered: v("1.3.0"),
                highest: v("1.4.0")
            }
        );
        assert!(hw.admit(&v("1.4.0")).is_ok(), "the same offer again");
        assert!(hw.admit(&v("1.5.0")).is_ok());
        // Kept across runs.
        let again = HighWater::at(file);
        assert_eq!(again.highest(), Some(v("1.4.0")));
        assert!(again.admit(&v("1.4.0-rc.1")).is_err());
    }

    #[test]
    fn the_mark_only_rises() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hw.json");
        let mut hw = HighWater::at(file.clone());
        hw.record(&v("2.0.0")).unwrap();
        hw.record(&v("1.0.0")).unwrap();
        assert_eq!(HighWater::read(&file), Some(v("2.0.0")));
    }

    #[test]
    fn a_missing_or_damaged_file_is_no_mark() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("updates.high-water.json");
        let hw = HighWater::at(file.clone());
        assert_eq!(hw.highest(), None);
        assert!(hw.admit(&v("0.0.1")).is_ok());
        for damaged in [
            &b"not json"[..],
            b"{\"highest\": 3}",
            b"{\"highest\": \"not a version\"}",
            b"{}",
            b"\xff\xfe",
            b"",
        ] {
            std::fs::write(&file, damaged).unwrap();
            assert_eq!(HighWater::read(&file), None);
            assert!(hw.admit(&v("0.0.1")).is_ok());
        }
        // A folder where the file should be: no mark, and recording says
        // so without panicking, keeping the mark for this run.
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir_all(file.join("x")).unwrap();
        let mut hw = HighWater::at(file);
        assert!(hw.record(&v("1.2.0")).is_err());
        assert!(hw.admit(&v("1.1.0")).is_err());
    }

    #[test]
    fn a_damaged_file_is_written_over_by_the_next_accepted_offer() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hw.json");
        std::fs::write(&file, "{{{").unwrap();
        let mut hw = HighWater::at(file.clone());
        hw.record(&v("1.1.0")).unwrap();
        assert_eq!(HighWater::read(&file), Some(v("1.1.0")));
    }

    #[test]
    fn in_memory_and_beside() {
        let mut hw = HighWater::in_memory();
        hw.record(&v("1.1.0")).unwrap();
        assert!(hw.admit(&v("1.0.9")).is_err());
        assert_eq!(
            HighWater::beside(Path::new("/data/updates.json")),
            Path::new("/data/updates.high-water.json")
        );
    }
}
