//! What each app tells the updater about itself: its name, its bundle's
//! name (and the one it had before, if it was renamed), the user agent it
//! sends, the endpoint it asks, the public key it trusts, and the sentences
//! it shows. Everything app-specific lives here, so the rest of the crate
//! serves every app alike.
//!
//! The endpoint and the key are baked in when the app compiles. A
//! rehearsal build (an app's own `rehearsal` feature) may bake a local
//! endpoint, a throwaway key and plain HTTP from compile-time variables;
//! every other build ignores those variables, so a release can never carry
//! them by accident. [`baked!`](crate::baked) reads them under the app's
//! own prefix, and [`Baked::resolve`] holds the rule.

use std::time::Duration;

use crate::target::Installer;
use crate::transport::Config;

/// GitHub serves a release's files from these hosts, after a redirect from
/// github.com.
pub const GITHUB_DOWNLOAD_HOSTS: [&str; 2] = [
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// The suffixes of the compile-time variables a rehearsal build reads,
/// after the app's prefix (`<PREFIX>_ENDPOINT` and so on). An app's
/// build.rs refuses them in any other build (README.md has the snippet).
pub const REHEARSAL_VARIABLES: [&str; 3] = ["_ENDPOINT", "_PUBKEY", "_INSECURE"];

/// The endpoint and key a build was compiled with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Baked {
    pub endpoint: &'static str,
    /// base64 of the minisign public key box, as `tauri.conf.json` carries it.
    pub pubkey: &'static str,
    /// Plain HTTP allowed: only a rehearsal build that asked for it.
    pub insecure: bool,
    /// Whether this is a rehearsal build at all.
    pub rehearsal: bool,
}

impl Baked {
    /// A release build: the app's own endpoint and key, HTTPS only.
    pub const fn release(endpoint: &'static str, pubkey: &'static str) -> Self {
        Self {
            endpoint,
            pubkey,
            insecure: false,
            rehearsal: false,
        }
    }

    /// The rule: only a rehearsal build takes the compile-time values, each
    /// one separately (an unset one keeps the release value), and plain
    /// HTTP only when the insecure variable is exactly `1`.
    pub fn resolve(
        rehearsal: bool,
        endpoint: &'static str,
        pubkey: &'static str,
        env_endpoint: Option<&'static str>,
        env_pubkey: Option<&'static str>,
        env_insecure: Option<&'static str>,
    ) -> Self {
        if !rehearsal {
            return Self::release(endpoint, pubkey);
        }
        Self {
            endpoint: env_endpoint.unwrap_or(endpoint),
            pubkey: env_pubkey.unwrap_or(pubkey),
            insecure: env_insecure == Some("1"),
            rehearsal: true,
        }
    }
}

/// The endpoint and key this app was compiled with, from its own release
/// values and, in a rehearsal build only, the compile-time variables
/// `<prefix>_ENDPOINT`, `<prefix>_PUBKEY` and `<prefix>_INSECURE`. It
/// expands in the app's crate, so the variables are read when the APP
/// compiles, and `rehearsal` is the app's own feature:
///
/// ```
/// const ENDPOINT: &str = "https://github.com/example/app/releases/latest/download/latest.json";
/// const PUBKEY: &str = "dW50cnVzdGVk";
/// let baked = method_update::baked!(
///     prefix = "EXAMPLE_APP_UPDATE",
///     rehearsal = false,
///     endpoint = ENDPOINT,
///     pubkey = PUBKEY,
/// );
/// assert_eq!(baked, method_update::Baked::release(ENDPOINT, PUBKEY));
/// ```
#[macro_export]
macro_rules! baked {
    (prefix = $prefix:literal, rehearsal = $rehearsal:expr, endpoint = $endpoint:expr, pubkey = $pubkey:expr $(,)?) => {
        $crate::Baked::resolve(
            $rehearsal,
            $endpoint,
            $pubkey,
            option_env!(concat!($prefix, "_ENDPOINT")),
            option_env!(concat!($prefix, "_PUBKEY")),
            option_env!(concat!($prefix, "_INSECURE")),
        )
    };
}

/// The names one app's Mac bundle may have: its current name, and the one
/// it had before a rename, if any. Copies still running under the old name
/// are moved to the current one by their next update, once; the startup
/// sweep treats both names as one bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleNames {
    /// The bundle's name, `.app` included (`Method Example.app`).
    pub current: String,
    /// The name it had before, if it was renamed.
    pub legacy: Option<String>,
}

impl BundleNames {
    pub fn new(current: impl Into<String>) -> Self {
        Self {
            current: current.into(),
            legacy: None,
        }
    }

    pub fn with_legacy(mut self, legacy: impl Into<String>) -> Self {
        self.legacy = Some(legacy.into());
        self
    }

    /// Whether `name` is the current or the legacy name.
    pub fn is_ours(&self, name: &str) -> bool {
        name == self.current || self.legacy.as_deref() == Some(name)
    }

    /// The names that stand for the bundle called `name`: both of ours when
    /// it is one of them, else `name` alone (a bundle someone renamed by
    /// hand stands for itself).
    pub fn standing_for<'a>(&'a self, name: &'a str) -> Vec<&'a str> {
        if self.is_ours(name) {
            let mut v = vec![self.current.as_str()];
            v.extend(self.legacy.as_deref());
            v
        } else {
            vec![name]
        }
    }
}

/// The sentences the updater itself may show, in the app's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Messages {
    /// A download larger than any build of the app could be.
    pub too_large: String,
    /// What every engine call is told once the bundle has been swapped
    /// ([`crate::gate::EngineGate`]).
    pub updated: String,
}

impl Messages {
    /// The suite's wording, with the app's name in it.
    pub fn for_app(display_name: &str) -> Self {
        Self {
            too_large: format!("The update is larger than any {display_name} could be."),
            updated: format!("{display_name} has been updated. Restart it to keep going."),
        }
    }
}

/// Everything the updater needs to know about one app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfig {
    /// The app's name as people see it (`Method Example`).
    pub display_name: String,
    pub bundle: BundleNames,
    /// Sent with every request (`method-example-update/1.2.3`).
    pub user_agent: String,
    /// Where `latest.json` is.
    pub endpoint: String,
    /// base64 of the minisign public key box.
    pub pubkey: String,
    /// Plain HTTP allowed: a rehearsal that asked for it, or a test.
    pub allow_insecure: bool,
    /// Hosts allowed besides the endpoint's own (and GitHub's download
    /// hosts, which come with a github.com endpoint).
    pub extra_hosts: Vec<String>,
    pub messages: Messages,
    /// The whole of one request, connect to last byte.
    pub timeout: Option<Duration>,
}

impl AppConfig {
    /// An app called `display_name`, whose bundle is `bundle_name`, sending
    /// `user_agent`, with the endpoint and key it was compiled with.
    pub fn new(
        display_name: impl Into<String>,
        bundle_name: impl Into<String>,
        user_agent: impl Into<String>,
        baked: Baked,
    ) -> Self {
        let display_name = display_name.into();
        Self {
            messages: Messages::for_app(&display_name),
            display_name,
            bundle: BundleNames::new(bundle_name),
            user_agent: user_agent.into(),
            endpoint: baked.endpoint.to_string(),
            pubkey: baked.pubkey.to_string(),
            allow_insecure: baked.insecure,
            extra_hosts: Vec::new(),
            timeout: Some(Duration::from_secs(10 * 60)),
        }
    }

    /// The bundle's name before a rename, so the next update moves it.
    pub fn with_legacy_bundle_name(mut self, legacy: impl Into<String>) -> Self {
        self.bundle = self.bundle.with_legacy(legacy);
        self
    }

    /// The hosts the updater may reach: the endpoint's, GitHub's two
    /// download hosts when the endpoint is on github.com, and any extra.
    pub fn allowed_hosts(&self) -> Vec<String> {
        crate::transport::allowed_hosts(std::slice::from_ref(&self.endpoint), &self.extra_hosts)
    }

    /// The transport settings for a check and a download: this app's
    /// endpoint, key, user agent and hosts; signed versions required.
    pub fn transport(&self, installer: Option<Installer>) -> Config {
        Config {
            endpoints: vec![self.endpoint.clone()],
            pubkey: self.pubkey.clone(),
            allow_insecure: self.allow_insecure,
            require_signed_version: true,
            timeout: self.timeout,
            installer,
            targets: None,
            user_agent: self.user_agent.clone(),
            allowed_hosts: self.allowed_hosts(),
            too_large: self.messages.too_large.clone(),
        }
    }

    /// Why this build cannot update itself, or `None` when it can: a key
    /// that is not a minisign public key (a placeholder), or an endpoint
    /// that is missing or not https. An app with a reason never checks.
    pub fn unavailable_reason(&self) -> Option<String> {
        if let Err(e) = crate::verify::decode_public_key(&self.pubkey) {
            return Some(format!("no updater key ({e})"));
        }
        if self.endpoint.trim().is_empty() {
            return Some("no update endpoint".into());
        }
        if !self.allow_insecure && !self.endpoint.to_ascii_lowercase().starts_with("https://") {
            return Some("the update endpoint isn't https".into());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENDPOINT: &str = "https://github.com/example/app/releases/latest/download/latest.json";

    #[test]
    fn a_release_build_ignores_every_rehearsal_value() {
        let b = Baked::resolve(
            false,
            ENDPOINT,
            "key",
            Some("http://127.0.0.1:1/latest.json"),
            Some("other"),
            Some("1"),
        );
        assert_eq!(b, Baked::release(ENDPOINT, "key"));
        assert!(!b.insecure);
    }

    #[test]
    fn a_rehearsal_takes_each_value_it_was_given() {
        let b = Baked::resolve(true, ENDPOINT, "key", None, Some("throwaway"), Some("1"));
        assert_eq!(b.endpoint, ENDPOINT);
        assert_eq!(b.pubkey, "throwaway");
        assert!(b.insecure && b.rehearsal);
        let b = Baked::resolve(true, ENDPOINT, "key", None, None, Some("yes"));
        assert!(!b.insecure, "only exactly 1 means plain HTTP");
    }

    #[test]
    fn the_names_stand_for_each_other() {
        let n = BundleNames::new("Method Example.app").with_legacy("Old Example.app");
        assert_eq!(
            n.standing_for("Old Example.app"),
            ["Method Example.app", "Old Example.app"]
        );
        assert_eq!(n.standing_for("Renamed.app"), ["Renamed.app"]);
        assert_eq!(BundleNames::new("A.app").standing_for("A.app"), ["A.app"]);
    }

    #[test]
    fn the_messages_carry_the_app_s_name() {
        let c = AppConfig::new(
            "Method Example",
            "Method Example.app",
            "method-example-update/1.0.0",
            Baked::release(ENDPOINT, "k"),
        );
        assert_eq!(
            c.messages.updated,
            "Method Example has been updated. Restart it to keep going."
        );
        assert_eq!(
            c.messages.too_large,
            "The update is larger than any Method Example could be."
        );
        assert_eq!(
            c.allowed_hosts(),
            [
                "github.com",
                "objects.githubusercontent.com",
                "release-assets.githubusercontent.com"
            ]
        );
        let t = c.transport(Some(Installer::App));
        assert!(t.require_signed_version);
        assert!(!t.allow_insecure);
        assert_eq!(t.user_agent, "method-example-update/1.0.0");
    }

    #[test]
    fn a_placeholder_key_or_a_plain_endpoint_means_no_checks() {
        let c = AppConfig::new("A", "A.app", "a/1", Baked::release(ENDPOINT, "placeholder"));
        assert!(
            c.unavailable_reason()
                .unwrap()
                .starts_with("no updater key")
        );
    }
}
