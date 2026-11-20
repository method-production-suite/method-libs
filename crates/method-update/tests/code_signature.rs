//! The code signature check inside the swap, with a stand-in for
//! `/usr/bin/codesign`: a new bundle signed by another team (or one whose
//! signature does not verify) is refused with a typed error before the
//! running bundle moves, through either path (this account's own swap or
//! the administrator's). An unsigned running app skips the check.

mod common;

use common::{CURRENT, EXE, archive_bundle, make_bundle, names};
use method_update::codesign::{CodeSignature, SignatureError, Signing};
use method_update::install::{InstallError, install_app_with};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

const TEAM: &str = "ABCDE12345";

/// The running bundle is signed by `running`; every other bundle (the
/// staged update) by `staged`, verifying when `valid`.
struct Tool {
    running_path: PathBuf,
    running: Signing,
    staged: Signing,
    valid: bool,
    asked: RefCell<Vec<PathBuf>>,
}

impl CodeSignature for Tool {
    fn signing(&self, bundle: &Path) -> Result<Signing, String> {
        self.asked.borrow_mut().push(bundle.to_path_buf());
        Ok(if bundle == self.running_path {
            self.running.clone()
        } else {
            self.staged.clone()
        })
    }
    fn verify(&self, _bundle: &Path, team: &str) -> Result<(), String> {
        assert_eq!(team, TEAM, "always the running app's team");
        if self.valid {
            Ok(())
        } else {
            Err("invalid signature (code or signature have been modified)".into())
        }
    }
}

fn no_admin(_: &Path, _: &Path) -> Result<(), InstallError> {
    panic!("no administrator should be needed in a temp folder")
}

fn version_of(bundle: &Path) -> String {
    std::fs::read_to_string(bundle.join("Contents/Resources/LICENSE")).unwrap()
}

fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn setup() -> (tempfile::TempDir, PathBuf, Vec<u8>) {
    let apps = tempfile::tempdir().unwrap();
    let bundle = apps.path().join(CURRENT);
    make_bundle(&bundle, EXE, "1.0.0");
    let build = tempfile::tempdir().unwrap();
    make_bundle(&build.path().join("Built.app"), EXE, "1.1.0");
    let archive = archive_bundle(&build.path().join("Built.app"));
    (apps, bundle, archive)
}

fn tool(bundle: &Path, running: Signing, staged: Signing, valid: bool) -> Tool {
    Tool {
        running_path: bundle.to_path_buf(),
        running,
        staged,
        valid,
        asked: RefCell::new(Vec::new()),
    }
}

#[test]
fn an_update_from_another_team_leaves_the_app_untouched() {
    let (apps, bundle, archive) = setup();
    let t = tool(
        &bundle,
        Signing::Team(TEAM.into()),
        Signing::Team("ZZZZZ99999".into()),
        true,
    );
    let err = install_app_with(&archive, &bundle, &names(), &no_admin, &t).unwrap_err();
    assert!(matches!(
        &err,
        InstallError::Signature(SignatureError::TeamMismatch { expected, found: Some(found) })
            if expected == TEAM && found == "ZZZZZ99999"
    ));
    assert!(err.to_string().contains("signed by team ZZZZZ99999"));
    assert_eq!(version_of(&bundle), "1.0.0");
    assert_eq!(listing(apps.path()), [CURRENT], "no staging left behind");
}

#[test]
fn a_signature_that_does_not_verify_leaves_the_app_untouched() {
    let (apps, bundle, archive) = setup();
    let t = tool(
        &bundle,
        Signing::Team(TEAM.into()),
        Signing::Team(TEAM.into()),
        false,
    );
    let err = install_app_with(&archive, &bundle, &names(), &no_admin, &t).unwrap_err();
    assert!(matches!(
        err,
        InstallError::Signature(SignatureError::Invalid { .. })
    ));
    assert_eq!(version_of(&bundle), "1.0.0");
    assert_eq!(listing(apps.path()), [CURRENT]);
}

#[test]
fn the_same_team_installs() {
    let (_apps, bundle, archive) = setup();
    let t = tool(
        &bundle,
        Signing::Team(TEAM.into()),
        Signing::Team(TEAM.into()),
        true,
    );
    let installed = install_app_with(&archive, &bundle, &names(), &no_admin, &t).unwrap();
    assert_eq!(version_of(&installed.bundle), "1.1.0");
    // The staged copy was asked about, beside the bundle, before the swap.
    assert_eq!(t.asked.borrow().len(), 2);
    assert_ne!(t.asked.borrow()[1], bundle);
}

#[test]
fn an_unsigned_running_app_skips_the_check() {
    let (_apps, bundle, archive) = setup();
    for running in [Signing::Unsigned, Signing::AdHoc] {
        // Even an unsigned update installs: nothing to compare against.
        let t = tool(&bundle, running, Signing::Unsigned, false);
        let installed = install_app_with(&archive, &bundle, &names(), &no_admin, &t).unwrap();
        assert_eq!(version_of(&installed.bundle), "1.1.0");
        assert_eq!(t.asked.borrow().len(), 1, "only the running app was read");
    }
}

#[cfg(unix)]
#[test]
fn the_administrator_path_checks_before_asking() {
    use std::os::unix::fs::PermissionsExt;
    let (apps, bundle, archive) = setup();
    let t = tool(
        &bundle,
        Signing::Team(TEAM.into()),
        Signing::Team("ZZZZZ99999".into()),
        true,
    );
    // A folder this account can't write sends the swap to an administrator.
    std::fs::set_permissions(apps.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let asked = std::cell::Cell::new(false);
    let admin = |_: &Path, _: &Path| -> Result<(), InstallError> {
        asked.set(true);
        Ok(())
    };
    let result = install_app_with(&archive, &bundle, &names(), &admin, &t);
    std::fs::set_permissions(apps.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        result,
        Err(InstallError::Signature(SignatureError::TeamMismatch { .. }))
    ));
    assert!(
        !asked.get(),
        "no administrator is asked for a refused update"
    );
    assert_eq!(version_of(&bundle), "1.0.0");
}
