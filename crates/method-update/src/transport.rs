//! The network half: the check and the download, over ureq and rustls.
//!
//! The check keeps the manifest format's own rules (tauri-plugin-updater
//! 2.12.0, `Updater::check`), so every app reads `latest.json` the same
//! way: every endpoint is tried in order; `{{current_version}}`,
//! `{{target}}`, `{{arch}}` and `{{bundle_type}}` are filled in; a 204
//! means "nothing new"; a non-success status is skipped without becoming
//! the error (so a lone 404 reads "Could not fetch a valid release JSON
//! from the remote"); a manifest that does not parse is the error; the
//! platform is looked up BEFORE the version is compared, so a build the
//! manifest has no row for always hears why; and newer means
//! semver-greater. The download is verified before anything sees it.
//!
//! The host rule (the suite's network standard): https only, and only to
//! the endpoint's host, GitHub's two download hosts when the endpoint is on
//! github.com, and any host the app adds. Redirects are followed here, one
//! hop at a time, so every hop meets the same rule; ureq's own redirect
//! handling is off.

use std::io::Read;
use std::time::Duration;

use semver::Version;
use ureq::http::{Response, Uri};

use crate::config::GITHUB_DOWNLOAD_HOSTS;
use crate::manifest::Release;
use crate::target::{Installer, TargetSet};
use crate::verify::{VerifyError, verify_signature};

/// No update archive is anywhere near this; a server that keeps sending is
/// refused rather than allowed to fill the disk.
pub const MAX_DOWNLOAD_BYTES: u64 = 1 << 30;

/// At most this many redirects for one request.
pub const MAX_REDIRECTS: usize = 10;

/// How one check or download is made. [`crate::AppConfig::transport`]
/// builds it for an app; tests build it directly.
#[derive(Clone, Debug)]
pub struct Config {
    pub endpoints: Vec<String>,
    /// base64 of the minisign public key box.
    pub pubkey: String,
    /// Plain http endpoints allowed (the format's
    /// `dangerousInsecureTransportProtocol`). Only a rehearsal or a test
    /// sets it.
    pub allow_insecure: bool,
    /// The signature must name the version the manifest announces. Every
    /// suite release is signed that way, so apps turn it on.
    pub require_signed_version: bool,
    /// The whole request, connect to last byte.
    pub timeout: Option<Duration>,
    /// How this copy was installed, for the `<os>-<arch>-<installer>` key.
    pub installer: Option<Installer>,
    /// Overrides the `<os>-<arch>` keys: tests only.
    pub targets: Option<TargetSet>,
    /// Sent with every request.
    pub user_agent: String,
    /// The only hosts any request (or redirect) may reach.
    pub allowed_hosts: Vec<String>,
    /// What [`UpdateError::TooLarge`] says.
    pub too_large: String,
}

impl Config {
    fn target_set(&self) -> Result<TargetSet, UpdateError> {
        match &self.targets {
            Some(t) => Ok(t.clone()),
            None => TargetSet::current(self.installer).map_err(UpdateError::Platform),
        }
    }

    fn agent(&self) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(self.timeout)
            .http_status_as_error(false)
            .https_only(!self.allow_insecure)
            .max_redirects(0)
            .user_agent(self.user_agent.as_str())
            .build()
            .into()
    }

    /// Refuses a URL the host rule does not allow.
    fn permit(&self, url: &str) -> Result<(), UpdateError> {
        if host_allowed(url, &self.allowed_hosts, self.allow_insecure) {
            Ok(())
        } else {
            Err(UpdateError::Host(url.to_string()))
        }
    }

    /// GET `url`, following redirects by hand: each hop must pass the host
    /// rule before it is requested.
    fn get(
        &self,
        agent: &ureq::Agent,
        url: &str,
        accept: &str,
    ) -> Result<Response<ureq::Body>, UpdateError> {
        let mut url = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            self.permit(&url)?;
            let res = agent
                .get(&url)
                .header("Accept", accept)
                .call()
                .map_err(|e| UpdateError::Network(e.to_string()))?;
            let status = res.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let location = res
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        UpdateError::Network(format!("A redirect from {url} named no location."))
                    })?;
                url = resolve(&url, location)
                    .ok_or_else(|| UpdateError::Host(location.to_string()))?;
                continue;
            }
            return Ok(res);
        }
        Err(UpdateError::Network(format!(
            "More than {MAX_REDIRECTS} redirects."
        )))
    }
}

/// The hosts the updater may reach: each endpoint's, GitHub's two download
/// hosts when an endpoint is on github.com, then `extra`. Lower case, no
/// repeats.
pub fn allowed_hosts(endpoints: &[String], extra: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    let mut add = |h: &str| {
        let h = h.to_ascii_lowercase();
        if !h.is_empty() && !hosts.contains(&h) {
            hosts.push(h);
        }
    };
    for e in endpoints {
        let Ok(uri) = e.parse::<Uri>() else {
            continue;
        };
        if let Some(h) = uri.host() {
            add(h);
            if h.eq_ignore_ascii_case("github.com") {
                for g in GITHUB_DOWNLOAD_HOSTS {
                    add(g);
                }
            }
        }
    }
    for h in extra {
        add(h);
    }
    hosts
}

/// Whether the updater may request `url`: https (or http when
/// `allow_insecure`), to one of `hosts`.
pub fn host_allowed(url: &str, hosts: &[String], allow_insecure: bool) -> bool {
    let Ok(uri) = url.parse::<Uri>() else {
        return false;
    };
    let scheme_ok = match uri.scheme_str() {
        Some(s) if s.eq_ignore_ascii_case("https") => true,
        Some(s) if s.eq_ignore_ascii_case("http") => allow_insecure,
        _ => false,
    };
    scheme_ok
        && uri
            .host()
            .is_some_and(|h| hosts.iter().any(|a| a.eq_ignore_ascii_case(h)))
}

/// A redirect's `Location` against the URL that sent it: absolute,
/// scheme-relative (`//host/path`), absolute-path (`/path`) or relative to
/// the current folder. None when the base cannot be read.
pub fn resolve(base: &str, location: &str) -> Option<String> {
    let has_scheme = location.split_once("://").is_some_and(|(s, _)| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    });
    if has_scheme {
        return Some(location.to_string());
    }
    let uri = base.parse::<Uri>().ok()?;
    let scheme = uri.scheme_str()?;
    if let Some(rest) = location.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}"));
    }
    let authority = uri.authority()?.as_str();
    if location.starts_with('/') {
        return Some(format!("{scheme}://{authority}{location}"));
    }
    let path = uri.path();
    let folder = path.rsplit_once('/').map_or("", |(f, _)| f);
    Some(format!("{scheme}://{authority}{folder}/{location}"))
}

/// Everything that can stop a check or a download, worded for a person.
#[derive(Debug)]
pub enum UpdateError {
    EmptyEndpoints,
    Insecure,
    /// A request or a redirect to a host the updater doesn't use.
    Host(String),
    Platform(String),
    Network(String),
    Manifest(String),
    ReleaseNotFound,
    Download(String),
    /// The app's own sentence ([`crate::Messages::too_large`]).
    TooLarge(String),
    Verify(VerifyError),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyEndpoints => f.write_str("Updater does not have any endpoints set."),
            Self::Insecure => f.write_str(
                "The configured updater endpoint must use a secure protocol like `https`.",
            ),
            Self::Host(url) => write!(f, "The updater does not use this address: {url}"),
            Self::Platform(s) | Self::Network(s) | Self::Manifest(s) | Self::TooLarge(s) => {
                f.write_str(s)
            }
            Self::ReleaseNotFound => {
                f.write_str("Could not fetch a valid release JSON from the remote")
            }
            // The format's updater shows its network errors in backticks.
            Self::Download(s) => write!(f, "`{s}`"),
            Self::Verify(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// What a check found: a release newer than this copy, for this platform.
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub current_version: String,
    pub version: String,
    pub body: Option<String>,
    pub date: Option<String>,
    /// The key that matched, e.g. `darwin-aarch64`.
    pub target: String,
    pub download_url: String,
    pub signature: String,
    pub raw_json: serde_json::Value,
}

/// Percent-encode controls and `+`, as the format's updater does for
/// `{{current_version}}` (build metadata carries a `+`).
fn encode_version(v: &str) -> String {
    let mut out = String::new();
    for b in v.bytes() {
        if b < 0x20 || b == 0x7f || b == b'+' || b >= 0x80 {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push(char::from(b));
        }
    }
    out
}

/// The endpoint with its placeholders filled in, raw or already encoded.
pub fn fill_endpoint(url: &str, current: &Version, targets: &TargetSet, installer: &str) -> String {
    let version = encode_version(&current.to_string());
    url.replace("%7B%7Bcurrent_version%7D%7D", &version)
        .replace("%7B%7Btarget%7D%7D", &targets.os)
        .replace("%7B%7Barch%7D%7D", &targets.arch)
        .replace("%7B%7Bbundle_type%7D%7D", installer)
        .replace("{{current_version}}", &version)
        .replace("{{target}}", &targets.os)
        .replace("{{arch}}", &targets.arch)
        .replace("{{bundle_type}}", installer)
}

/// The check: `Ok(None)` when nothing is newer (or the server said 204),
/// `Ok(Some)` for a newer release this platform has a file in.
pub fn check(config: &Config, current: &Version) -> Result<Option<Update>, UpdateError> {
    if config.endpoints.is_empty() {
        return Err(UpdateError::EmptyEndpoints);
    }
    if !config.allow_insecure
        && config
            .endpoints
            .iter()
            .any(|e| !e.to_ascii_lowercase().starts_with("https://"))
    {
        return Err(UpdateError::Insecure);
    }
    let targets = config.target_set()?;
    let installer = config.installer.map(Installer::name).unwrap_or("unknown");
    let agent = config.agent();

    let mut found: Option<(Release, serde_json::Value)> = None;
    let mut last_error: Option<UpdateError> = None;
    for endpoint in &config.endpoints {
        let url = fill_endpoint(endpoint, current, &targets, installer);
        match config.get(&agent, &url, "application/json") {
            Ok(mut res) => {
                let status = res.status().as_u16();
                if !(200..300).contains(&status) {
                    continue;
                }
                if status == 204 {
                    return Ok(None);
                }
                let text = match res.body_mut().read_to_string() {
                    Ok(t) => t,
                    Err(e) => {
                        last_error = Some(UpdateError::Network(e.to_string()));
                        continue;
                    }
                };
                let parsed = serde_json::from_str::<serde_json::Value>(&text)
                    .map_err(|e| UpdateError::Manifest(e.to_string()))
                    .and_then(|raw| {
                        Release::from_value(raw.clone())
                            .map(|r| (r, raw))
                            .map_err(UpdateError::Manifest)
                    });
                match parsed {
                    Ok(pair) => {
                        last_error = None;
                        found = Some(pair);
                        break;
                    }
                    Err(e) => last_error = Some(e),
                }
            }
            Err(e) => last_error = Some(e),
        }
    }
    if let Some(e) = last_error {
        return Err(e);
    }
    let (release, raw_json) = found.ok_or(UpdateError::ReleaseNotFound)?;
    let platform = targets.pick(&release).map_err(UpdateError::Platform)?;
    if release.version <= *current {
        return Ok(None);
    }
    let target = targets
        .keys
        .iter()
        .find(|k| release.platform(k).is_ok())
        .cloned()
        .unwrap_or_default();
    Ok(Some(Update {
        current_version: current.to_string(),
        version: release.version.to_string(),
        body: release.notes.clone(),
        date: release.pub_date.clone(),
        target,
        download_url: platform.url.clone(),
        signature: platform.signature.clone(),
        raw_json,
    }))
}

/// The download: the bytes, reported as they arrive
/// (`on_chunk(chunk_len, content_length)`), then `on_finish`, then
/// verified. Nothing unverified is ever returned. A download URL on a host
/// the updater doesn't use is refused before anything is requested.
pub fn download(
    config: &Config,
    update: &Update,
    mut on_chunk: impl FnMut(usize, Option<u64>),
    on_finish: impl FnOnce(),
) -> Result<Vec<u8>, UpdateError> {
    let agent = config.agent();
    let mut res = config.get(&agent, &update.download_url, "application/octet-stream")?;
    let status = res.status();
    if !status.is_success() {
        let reason = status.canonical_reason().unwrap_or("");
        return Err(UpdateError::Download(
            format!(
                "Download request failed with status: {} {reason}",
                status.as_u16()
            )
            .trim_end()
            .to_string(),
        ));
    }
    let length: Option<u64> = res
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok());
    if length.is_some_and(|n| n > MAX_DOWNLOAD_BYTES) {
        return Err(UpdateError::TooLarge(config.too_large.clone()));
    }
    let mut reader = res.body_mut().as_reader();
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|e| UpdateError::Network(e.to_string()))?;
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(chunk.get(..n).unwrap_or_default());
        if buffer.len() as u64 > MAX_DOWNLOAD_BYTES {
            return Err(UpdateError::TooLarge(config.too_large.clone()));
        }
        on_chunk(n, length);
    }
    on_finish();
    verify_signature(
        &buffer,
        &update.signature,
        &config.pubkey,
        &update.version,
        config.require_signed_version,
    )
    .map_err(UpdateError::Verify)?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        crate::AppConfig::new(
            "Method Example",
            "Method Example.app",
            "method-example-update/1.0.0",
            crate::Baked::release(
                "https://github.com/example/app/releases/latest/download/latest.json",
                "k",
            ),
        )
        .transport(Some(Installer::App))
    }

    #[test]
    fn placeholders_are_filled_as_the_format_fills_them() {
        let t = TargetSet::new("darwin", "aarch64", Some(Installer::App));
        let v = Version::parse("0.11.1+build.7").unwrap();
        assert_eq!(
            fill_endpoint(
                "https://x/{{target}}/{{arch}}/{{current_version}}?b={{bundle_type}}",
                &v,
                &t,
                "app"
            ),
            "https://x/darwin/aarch64/0.11.1%2Bbuild.7?b=app"
        );
        assert_eq!(
            fill_endpoint("https://x/%7B%7Btarget%7D%7D", &v, &t, "app"),
            "https://x/darwin"
        );
    }

    #[test]
    fn a_plain_http_endpoint_is_refused_unless_allowed() {
        let mut c = config();
        c.endpoints = vec!["http://127.0.0.1:9/latest.json".into()];
        c.allow_insecure = false;
        let err = check(&c, &Version::new(0, 11, 1)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "The configured updater endpoint must use a secure protocol like `https`."
        );
        c.endpoints.clear();
        assert!(matches!(
            check(&c, &Version::new(0, 11, 1)),
            Err(UpdateError::EmptyEndpoints)
        ));
    }

    #[test]
    fn only_the_endpoint_s_hosts_over_https() {
        let hosts = config().allowed_hosts;
        for ok in [
            "https://github.com/example/app/releases/download/v1/a.tar.gz",
            "https://objects.githubusercontent.com/x",
            "https://release-assets.githubusercontent.com/x",
            "HTTPS://GitHub.com/x",
        ] {
            assert!(host_allowed(ok, &hosts, false), "{ok}");
        }
        for bad in [
            "http://github.com/x",
            "https://example.com/x",
            "https://github.com.example.com/x",
            "https://evil.githubusercontent.com/x",
            "ftp://github.com/x",
            "not a url",
        ] {
            assert!(!host_allowed(bad, &hosts, false), "{bad}");
        }
        assert!(host_allowed("http://github.com/x", &hosts, true));
        // A host that is not github.com brings no GitHub hosts with it.
        assert_eq!(
            allowed_hosts(&["https://updates.example.org/latest.json".into()], &[]),
            ["updates.example.org"]
        );
        assert_eq!(
            allowed_hosts(&["https://a.example/x".into()], &["B.example".into()]),
            ["a.example", "b.example"]
        );
    }

    #[test]
    fn redirect_locations_resolve_against_the_hop_that_sent_them() {
        let base = "https://github.com/example/app/releases/latest/download/latest.json";
        assert_eq!(
            resolve(base, "https://objects.githubusercontent.com/a?b=c").unwrap(),
            "https://objects.githubusercontent.com/a?b=c"
        );
        assert_eq!(
            resolve(base, "//release-assets.githubusercontent.com/a").unwrap(),
            "https://release-assets.githubusercontent.com/a"
        );
        assert_eq!(
            resolve(base, "/example/app/releases/download/v1/latest.json").unwrap(),
            "https://github.com/example/app/releases/download/v1/latest.json"
        );
        assert_eq!(
            resolve(base, "other.json").unwrap(),
            "https://github.com/example/app/releases/latest/download/other.json"
        );
    }
}
