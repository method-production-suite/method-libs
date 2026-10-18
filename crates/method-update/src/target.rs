//! Which `platforms` key a build answers to. The manifest format's own
//! updater (tauri-plugin-updater 2.12.0, `get_urls`) tries
//! `<os>-<arch>-<installer>` first and then `<os>-<arch>`; this crate does
//! the same, so the keys the release pipelines write keep working.
//!
//! The suite updates Apple silicon Macs only (SUITE-STANDARDS.md, section
//! 1), so the only key a real build asks for is `darwin-aarch64` (after
//! `darwin-aarch64-app`). Any other build has no key: a check says so, in
//! the format's own words, rather than offering it a file it cannot run.
//! Tests name other keys through [`TargetSet::new`].

use crate::manifest::{Platform, Release};

/// How the running copy was installed, as the format names it. A suite
/// app on a Mac is an `.app` bundle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Installer {
    App,
}

impl Installer {
    pub fn name(self) -> &'static str {
        match self {
            Self::App => "app",
        }
    }
}

/// The keys one build looks for, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetSet {
    pub os: String,
    pub arch: String,
    pub keys: Vec<String>,
}

impl TargetSet {
    pub fn new(os: &str, arch: &str, installer: Option<Installer>) -> Self {
        let mut keys = Vec::new();
        if let Some(i) = installer {
            keys.push(format!("{os}-{arch}-{}", i.name()));
        }
        keys.push(format!("{os}-{arch}"));
        Self {
            os: os.into(),
            arch: arch.into(),
            keys,
        }
    }

    /// This build, installed as `installer`: an Apple silicon Mac, or an
    /// error saying updates are for those only.
    pub fn current(installer: Option<Installer>) -> Result<Self, String> {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Ok(Self::new("darwin", "aarch64", installer))
        } else {
            Err("Updates are for Apple silicon Macs only (`darwin-aarch64`).".into())
        }
    }

    /// The first key the manifest has, or the format's `TargetsNotFound`
    /// words, which an app can read to tell an Intel Mac why it is offered
    /// nothing.
    pub fn pick<'a>(&self, release: &'a Release) -> Result<&'a Platform, String> {
        for key in &self.keys {
            if let Ok(p) = release.platform(key) {
                return Ok(p);
            }
        }
        Err(format!(
            "None of the fallback platforms `{:?}` were found in the response `platforms` object",
            self.keys
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(keys: &[&str]) -> Release {
        let platforms: serde_json::Map<String, serde_json::Value> = keys
            .iter()
            .map(|k| {
                (
                    k.to_string(),
                    serde_json::json!({"url": format!("https://x/{k}"), "signature": "s"}),
                )
            })
            .collect();
        Release::from_value(serde_json::json!({"version": "1.0.0", "platforms": platforms}))
            .unwrap()
    }

    #[test]
    fn the_installer_key_comes_first_then_the_plain_one() {
        let t = TargetSet::new("darwin", "aarch64", Some(Installer::App));
        assert_eq!(t.keys, ["darwin-aarch64-app", "darwin-aarch64"]);
        let both = release(&["darwin-aarch64", "darwin-aarch64-app"]);
        assert_eq!(t.pick(&both).unwrap().url, "https://x/darwin-aarch64-app");
        let plain = release(&["darwin-aarch64"]);
        assert_eq!(t.pick(&plain).unwrap().url, "https://x/darwin-aarch64");
    }

    #[test]
    fn an_intel_mac_gets_the_format_s_words() {
        let t = TargetSet::new("darwin", "x86_64", Some(Installer::App));
        let msg = t.pick(&release(&["darwin-aarch64"])).unwrap_err();
        assert_eq!(
            msg,
            "None of the fallback platforms `[\"darwin-x86_64-app\", \"darwin-x86_64\"]` were found in the response `platforms` object"
        );
    }

    #[test]
    fn only_an_apple_silicon_mac_has_a_key() {
        let t = TargetSet::current(None);
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert_eq!(t.unwrap().keys, ["darwin-aarch64"]);
        } else {
            assert!(t.unwrap_err().contains("Apple silicon"));
        }
    }
}
