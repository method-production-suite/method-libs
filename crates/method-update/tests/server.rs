//! The whole update, against a local fake of the release endpoint and a
//! throwaway minisign key: check, download, verify, swap, restart command,
//! and the host rule on every request and redirect. The manifest and the
//! archive have the shape the suite's release pipelines publish.

mod common;

use common::{
    Answer, CURRENT, EXE, LEGACY, LEGACY_EXE, Server, TestKey, archive_bundle, make_bundle, names,
    tar_gz,
};
use method_update::install::{InstallError, install_app};
use method_update::target::{Installer, TargetSet};
use method_update::{AppConfig, Baked, Config, UpdateError, check, download};
use semver::Version;

const ARCHIVE: &str = "/download/v1.3.0/Method-Example-macOS.app.tar.gz";

/// The example app as a rehearsal build would bake it: the local server's
/// endpoint, the throwaway key, plain HTTP; the server's host allowed.
fn config(server: &Server, key: &TestKey) -> Config {
    let endpoint: &'static str =
        Box::leak(server.url("/latest/download/latest.json").into_boxed_str());
    let pubkey: &'static str = Box::leak(key.pubkey_b64().into_boxed_str());
    let app = AppConfig::new(
        "Method Example",
        CURRENT,
        "method-example-update/1.1.0",
        Baked::resolve(
            true,
            "https://unused.invalid/",
            "unused",
            Some(endpoint),
            Some(pubkey),
            Some("1"),
        ),
    )
    .with_legacy_bundle_name(LEGACY);
    assert_eq!(app.allowed_hosts(), ["127.0.0.1"]);
    let mut c = app.transport(Some(Installer::App));
    c.timeout = Some(std::time::Duration::from_secs(10));
    c.targets = Some(TargetSet::new("darwin", "aarch64", Some(Installer::App)));
    c
}

fn manifest(server: &Server, version: &str, signature: &str) -> String {
    serde_json::json!({
        "version": version,
        "notes": format!("Method Example {version}. Release notes: https://example.invalid/v{version}"),
        "pub_date": "2026-10-12T09:00:00.000Z",
        "platforms": {
            "darwin-aarch64": { "url": server.url(ARCHIVE), "signature": signature }
        }
    })
    .to_string()
}

fn no_admin(_: &std::path::Path, _: &std::path::Path) -> Result<(), InstallError> {
    panic!("no administrator should be needed in a temp folder")
}

struct World {
    server: Server,
    key: TestKey,
    apps: tempfile::TempDir,
    archive: Vec<u8>,
}

/// A bundle installed under the legacy name, with a helper beside its
/// executable; the current build published as 1.3.0.
fn world() -> World {
    let server = Server::start();
    let key = TestKey::new();
    let apps = tempfile::tempdir().unwrap();
    make_bundle(&apps.path().join(LEGACY), LEGACY_EXE, "1.1.1");
    std::fs::write(
        apps.path().join(LEGACY).join("Contents/MacOS/old-helper"),
        "old helper",
    )
    .unwrap();
    let build = tempfile::tempdir().unwrap();
    make_bundle(&build.path().join("Built.app"), EXE, "1.3.0");
    let archive = archive_bundle(&build.path().join("Built.app"));
    World {
        server,
        key,
        apps,
        archive,
    }
}

impl World {
    fn publish(&self, announced: &str, signed_for: Option<&str>, bytes: &[u8]) {
        let sig = self
            .key
            .sign(bytes, "Method-Example-macOS.app.tar.gz", signed_for);
        self.server.route(
            "/latest/download/latest.json",
            Answer::json(&manifest(&self.server, announced, &sig)),
        );
        self.server
            .route(ARCHIVE, Answer::bytes(self.archive.clone()));
    }

    /// The installed bundle, under its legacy name.
    fn bundle(&self) -> std::path::PathBuf {
        self.apps.path().join(LEGACY)
    }

    /// Where the update goes: the one-time rename.
    fn renamed(&self) -> std::path::PathBuf {
        self.apps.path().join(CURRENT)
    }

    /// The executables of whichever bundle is there.
    fn exe_names(&self) -> Vec<String> {
        let bundle = if self.bundle().exists() {
            self.bundle()
        } else {
            self.renamed()
        };
        let mut v: Vec<String> = std::fs::read_dir(bundle.join("Contents/MacOS"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
}

#[test]
fn a_legacy_bundle_is_updated_under_its_current_name() {
    let w = world();
    w.publish("1.3.0", Some("1.3.0"), &w.archive.clone());
    let c = config(&w.server, &w.key);

    let update = check(&c, &Version::new(1, 1, 1)).unwrap().unwrap();
    assert_eq!(update.version, "1.3.0");
    assert_eq!(update.current_version, "1.1.1");
    assert_eq!(update.target, "darwin-aarch64");
    assert!(
        update
            .body
            .as_deref()
            .unwrap()
            .starts_with("Method Example 1.3.0.")
    );

    let mut received = 0usize;
    let mut finished = false;
    let bytes = download(
        &c,
        &update,
        |n, total| {
            received += n;
            assert_eq!(total, Some(w.archive.len() as u64));
        },
        || finished = true,
    )
    .unwrap();
    assert!(finished);
    assert_eq!(received, w.archive.len());

    let installed = install_app(&bytes, &w.bundle(), &names(), &no_admin).unwrap();
    assert_eq!(installed.bundle, w.renamed());
    assert_eq!(installed.leftover, None);
    // New name, new contents: the old bundle, its executable and its
    // helper are gone.
    assert!(!w.bundle().exists());
    assert_eq!(w.exe_names(), [EXE]);
    // The swap takes the archive's Info.plist whole; the identifier changed
    // and nothing minded (the updater never checks it).
    assert_eq!(
        method_update::install::bundle_identifier(&w.renamed()).as_deref(),
        Some("org.example.method-example")
    );
    // Nothing left beside it.
    let beside: Vec<_> = std::fs::read_dir(w.apps.path()).unwrap().collect();
    assert_eq!(beside.len(), 1);
    // The restart reads the NEW Info.plist, at the path the install
    // reported.
    let cmd = method_update::restart::relaunch_command(&installed.bundle, &[]).unwrap();
    assert_eq!(
        std::path::Path::new(cmd.get_program()),
        w.renamed().join("Contents/MacOS").join(EXE)
    );

    let seen = w.server.seen.lock().unwrap().clone();
    assert_eq!(
        seen[0],
        (
            "/latest/download/latest.json".into(),
            "application/json".into()
        )
    );
    assert_eq!(seen[1], (ARCHIVE.into(), "application/octet-stream".into()));
}

#[test]
fn a_tampered_archive_is_refused_and_nothing_moves() {
    let w = world();
    let mut evil = w.archive.clone();
    let sig = w.key.sign(&w.archive, "x", Some("1.3.0"));
    let last = evil.len() - 9;
    evil[last] ^= 1;
    w.server.route(
        "/latest/download/latest.json",
        Answer::json(&manifest(&w.server, "1.3.0", &sig)),
    );
    w.server.route(ARCHIVE, Answer::bytes(evil));
    let c = config(&w.server, &w.key);
    let update = check(&c, &Version::new(1, 1, 1)).unwrap().unwrap();
    let err = download(&c, &update, |_, _| {}, || {}).unwrap_err();
    assert!(matches!(err, UpdateError::Verify(_)), "{err}");
    assert_eq!(w.exe_names(), [LEGACY_EXE, "old-helper"]);
}

#[test]
fn another_key_is_refused() {
    let w = world();
    w.publish("1.3.0", Some("1.3.0"), &w.archive.clone());
    let mut c = config(&w.server, &w.key);
    c.pubkey = TestKey::new().pubkey_b64();
    let update = check(&c, &Version::new(1, 1, 1)).unwrap().unwrap();
    assert!(download(&c, &update, |_, _| {}, || {}).is_err());
}

#[test]
fn a_manifest_announcing_another_version_than_the_signature_is_refused() {
    let w = world();
    w.publish("1.3.1", Some("1.3.0"), &w.archive.clone());
    let c = config(&w.server, &w.key);
    let update = check(&c, &Version::new(1, 1, 1)).unwrap().unwrap();
    let err = download(&c, &update, |_, _| {}, || {}).unwrap_err();
    assert!(err.to_string().starts_with(
        "The update was signed for version 1.3.0 but the update endpoint announced version 1.3.1."
    ));
}

#[test]
fn a_signature_without_a_version_passes_only_when_not_required() {
    let w = world();
    w.publish("1.3.0", None, &w.archive.clone());
    let mut c = config(&w.server, &w.key);
    let update = check(&c, &Version::new(1, 1, 1)).unwrap().unwrap();
    // Every suite app requires the signed version...
    assert!(download(&c, &update, |_, _| {}, || {}).is_err());
    // ...the format's default does not.
    c.require_signed_version = false;
    assert!(download(&c, &update, |_, _| {}, || {}).is_ok());
}

#[test]
fn nothing_newer_no_content_and_a_missing_manifest() {
    let w = world();
    w.publish("1.1.1", Some("1.1.1"), &w.archive.clone());
    let c = config(&w.server, &w.key);
    assert_eq!(check(&c, &Version::new(1, 1, 1)).unwrap(), None);
    // A dev build past the tag: semver says 1.1.1 > 1.1.1-3-gabc, which is
    // why an app may apply its own comparator on top.
    assert!(
        check(&c, &Version::parse("1.1.1-3-gabc").unwrap())
            .unwrap()
            .is_some()
    );

    w.server
        .route("/latest/download/latest.json", Answer::status(204));
    assert_eq!(check(&c, &Version::new(1, 1, 1)).unwrap(), None);

    w.server
        .route("/latest/download/latest.json", Answer::status(404));
    assert_eq!(
        check(&c, &Version::new(1, 1, 1)).unwrap_err().to_string(),
        "Could not fetch a valid release JSON from the remote"
    );

    w.server
        .route("/latest/download/latest.json", Answer::json("{\"oops\":1}"));
    assert!(matches!(
        check(&c, &Version::new(1, 1, 1)),
        Err(UpdateError::Manifest(_))
    ));
}

#[test]
fn an_intel_mac_hears_why_even_when_nothing_is_newer() {
    let w = world();
    w.publish("1.1.1", Some("1.1.1"), &w.archive.clone());
    let mut c = config(&w.server, &w.key);
    c.targets = Some(TargetSet::new("darwin", "x86_64", Some(Installer::App)));
    let err = check(&c, &Version::new(0, 9, 0)).unwrap_err().to_string();
    assert!(err.contains("darwin-x86_64"));
    assert!(err.contains("`platforms` object"));
}

#[test]
fn a_missing_archive_says_so_with_the_status() {
    let w = world();
    w.publish("1.3.0", Some("1.3.0"), &w.archive.clone());
    w.server.route(ARCHIVE, Answer::status(404));
    let c = config(&w.server, &w.key);
    let update = check(&c, &Version::new(1, 1, 1)).unwrap().unwrap();
    assert_eq!(
        download(&c, &update, |_, _| {}, || {})
            .unwrap_err()
            .to_string(),
        "`Download request failed with status: 404 Not Found`"
    );
}

#[test]
fn an_archive_that_is_not_a_bundle_never_moves_the_old_one() {
    let w = world();
    let no_plist = tar_gz(&[
        ("Method Example.app/", b"", 0o755),
        ("Method Example.app/Contents/MacOS/x", b"x", 0o755),
    ]);
    let err = install_app(&no_plist, &w.bundle(), &names(), &no_admin).unwrap_err();
    assert!(err.to_string().contains("Info.plist"), "{err}");
    assert_eq!(w.exe_names(), [LEGACY_EXE, "old-helper"]);

    let escape = tar_gz(&[("Method Example.app/../../evil", b"x", 0o644)]);
    let err = install_app(&escape, &w.bundle(), &names(), &no_admin).unwrap_err();
    assert!(err.to_string().contains("outside its bundle"), "{err}");
    assert!(!w.apps.path().parent().unwrap().join("evil").exists());
    assert_eq!(w.exe_names(), [LEGACY_EXE, "old-helper"]);
    let beside: Vec<_> = std::fs::read_dir(w.apps.path()).unwrap().collect();
    assert_eq!(beside.len(), 1, "no staging folder left behind");
}

#[cfg(unix)]
#[test]
fn an_old_bundle_that_cannot_be_deleted_is_left_over_not_a_failure() {
    use std::os::unix::fs::PermissionsExt;
    let w = world();
    let locked = w.bundle().join("Contents/Resources/locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(locked.join("f"), "x").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

    let installed = install_app(&w.archive, &w.bundle(), &names(), &no_admin).unwrap();
    let leftover = installed.leftover.expect("the old bundle stays behind");
    assert_eq!(w.exe_names(), [EXE]);
    std::fs::set_permissions(
        leftover.join("Contents/Resources/locked"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn the_executable_bit_survives_the_round_trip() {
    use std::os::unix::fs::PermissionsExt;
    let w = world();
    let installed = install_app(&w.archive, &w.bundle(), &names(), &no_admin).unwrap();
    let mode = std::fs::metadata(installed.bundle.join("Contents/MacOS/method-example"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111);
}

// ---- the host rule: https only, the endpoint's host, redirects included ----

#[test]
fn a_redirect_on_the_same_host_is_followed() {
    let w = world();
    w.publish("1.3.0", Some("1.3.0"), &w.archive.clone());
    let manifest = w.server.routes.lock().unwrap()["/latest/download/latest.json"].clone();
    w.server.route("/v1.3.0/latest.json", manifest);
    w.server.route(
        "/latest/download/latest.json",
        Answer::redirect("/v1.3.0/latest.json"),
    );
    let c = config(&w.server, &w.key);
    let update = check(&c, &Version::new(1, 1, 1)).unwrap().unwrap();
    assert_eq!(update.version, "1.3.0");
    let seen: Vec<String> = w
        .server
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.0.clone())
        .collect();
    assert_eq!(
        seen,
        ["/latest/download/latest.json", "/v1.3.0/latest.json"]
    );
}

#[test]
fn a_redirect_to_another_host_is_refused_before_it_is_followed() {
    let w = world();
    w.publish("1.3.0", Some("1.3.0"), &w.archive.clone());
    w.server.route(
        "/latest/download/latest.json",
        Answer::redirect(&w.server.other_host_url("/elsewhere/latest.json")),
    );
    let c = config(&w.server, &w.key);
    let err = check(&c, &Version::new(1, 1, 1)).unwrap_err();
    assert!(matches!(err, UpdateError::Host(_)), "{err}");
    assert!(err.to_string().contains("localhost"), "{err}");
    let seen = w.server.seen.lock().unwrap().len();
    assert_eq!(seen, 1, "the other host was never asked");
}

#[test]
fn a_download_on_another_host_is_refused_and_never_requested() {
    let w = world();
    let sig = w.key.sign(&w.archive, "x", Some("1.3.0"));
    let manifest = serde_json::json!({
        "version": "1.3.0",
        "platforms": {
            "darwin-aarch64": { "url": w.server.other_host_url(ARCHIVE), "signature": sig }
        }
    });
    w.server.route(
        "/latest/download/latest.json",
        Answer::json(&manifest.to_string()),
    );
    w.server.route(ARCHIVE, Answer::bytes(w.archive.clone()));
    let c = config(&w.server, &w.key);
    let update = check(&c, &Version::new(1, 1, 1)).unwrap().unwrap();
    let err = download(&c, &update, |_, _| {}, || {}).unwrap_err();
    assert!(matches!(err, UpdateError::Host(_)), "{err}");
    assert_eq!(w.server.seen.lock().unwrap().len(), 1, "only the manifest");
}

#[test]
fn a_download_redirected_off_the_allowed_hosts_is_refused() {
    let w = world();
    w.publish("1.3.0", Some("1.3.0"), &w.archive.clone());
    w.server.route(
        ARCHIVE,
        Answer::redirect(&w.server.other_host_url("/asset")),
    );
    let c = config(&w.server, &w.key);
    let update = check(&c, &Version::new(1, 1, 1)).unwrap().unwrap();
    let err = download(&c, &update, |_, _| {}, || {}).unwrap_err();
    assert!(matches!(err, UpdateError::Host(_)), "{err}");
}

#[test]
fn a_redirect_loop_gives_up() {
    let w = world();
    w.server.route(
        "/latest/download/latest.json",
        Answer::redirect("/latest/download/latest.json"),
    );
    let c = config(&w.server, &w.key);
    let err = check(&c, &Version::new(1, 1, 1)).unwrap_err();
    assert!(err.to_string().contains("redirects"), "{err}");
}

#[test]
fn a_release_build_s_config_refuses_plain_http_even_on_its_own_host() {
    let w = world();
    w.publish("1.3.0", Some("1.3.0"), &w.archive.clone());
    let mut c = config(&w.server, &w.key);
    c.allow_insecure = false;
    assert!(matches!(
        check(&c, &Version::new(1, 1, 1)),
        Err(UpdateError::Insecure)
    ));
    assert!(w.server.seen.lock().unwrap().is_empty());
}

#[test]
fn the_policy_updater_runs_the_whole_update_against_the_server() {
    let w = world();
    w.publish("1.3.0", Some("1.3.0"), &w.archive.clone());
    let c = config(&w.server, &w.key);
    let current = Version::new(1, 1, 1);
    let updater = method_update::Updater::new(
        AppConfig::new(
            "Method Example",
            CURRENT,
            "method-example-update/1.1.1",
            Baked::release(
                "https://github.com/example/app/releases/latest/download/latest.json",
                Box::leak(w.key.pubkey_b64().into_boxed_str()),
            ),
        )
        .with_legacy_bundle_name(LEGACY),
        "1.1.1",
        None,
        std::time::Instant::now(),
    );
    let said = updater.check_with(
        false,
        || check(&c, &current).map_err(|e| e.to_string()),
        |u| download(&c, u, |_, _| {}, || {}).map_err(|e| e.to_string()),
    );
    assert_eq!(said, None);
    assert_eq!(updater.status(None).phase, "ready");
    let installed = updater.install_ready(&w.bundle(), &no_admin).unwrap();
    assert_eq!(installed.bundle, w.renamed());
    assert_eq!(w.exe_names(), [EXE]);
}
