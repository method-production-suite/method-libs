//! The one-time bundle rename, for an app with a legacy bundle name. A copy
//! still running under the old name (`Old Example.app`) installs its next
//! version as `Method Example.app` beside it, removes the old bundle, and
//! the restart starts the new path. A bundle with the current name already
//! there (a fresh download beside an old copy) is never touched: the old
//! bundle then updates in place. An app with no legacy name never renames.

mod common;

use common::{CURRENT, EXE, LEGACY, LEGACY_EXE, archive_bundle, make_bundle, names, tar_gz};
use method_update::BundleNames;
use method_update::install::{InstallError, install_app, install_destination};
use std::path::Path;

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

fn new_archive(version: &str) -> Vec<u8> {
    let build = tempfile::tempdir().unwrap();
    make_bundle(&build.path().join("Built.app"), EXE, version);
    archive_bundle(&build.path().join("Built.app"))
}

#[test]
fn an_old_bundle_alone_is_installed_under_the_current_name() {
    let apps = tempfile::tempdir().unwrap();
    let old = apps.path().join(LEGACY);
    make_bundle(&old, LEGACY_EXE, "1.3.0");
    let new = apps.path().join(CURRENT);
    assert_eq!(install_destination(&old, &names()), new);

    let installed = install_app(&new_archive("1.3.1"), &old, &names(), &no_admin).unwrap();
    assert_eq!(installed.bundle, new);
    assert_eq!(installed.leftover, None);
    assert_eq!(version_of(&new), "1.3.1");
    // The old bundle is gone and nothing hidden is left beside the new one.
    assert_eq!(listing(apps.path()), [CURRENT]);

    // The restart starts the NEW path's executable, read from the NEW
    // Info.plist.
    let cmd = method_update::restart::relaunch_command(&installed.bundle, &[]).unwrap();
    assert_eq!(
        Path::new(cmd.get_program()),
        new.join("Contents/MacOS").join(EXE)
    );
}

#[test]
fn without_a_legacy_name_nothing_is_ever_renamed() {
    let apps = tempfile::tempdir().unwrap();
    let old = apps.path().join(LEGACY);
    make_bundle(&old, LEGACY_EXE, "1.3.0");
    let plain = BundleNames::new(CURRENT);
    assert_eq!(install_destination(&old, &plain), old);
    let installed = install_app(&new_archive("1.3.1"), &old, &plain, &no_admin).unwrap();
    assert_eq!(installed.bundle, old);
    assert_eq!(listing(apps.path()), [LEGACY]);
}

#[test]
fn a_current_bundle_already_beside_it_is_never_touched() {
    let apps = tempfile::tempdir().unwrap();
    let old = apps.path().join(LEGACY);
    make_bundle(&old, LEGACY_EXE, "1.3.0");
    let other = apps.path().join(CURRENT);
    make_bundle(&other, EXE, "1.4.0");
    assert_eq!(install_destination(&old, &names()), old);

    let installed = install_app(&new_archive("1.3.1"), &old, &names(), &no_admin).unwrap();
    assert_eq!(installed.bundle, old, "the old path updates in place");
    assert_eq!(version_of(&old), "1.3.1");
    assert_eq!(version_of(&other), "1.4.0", "the other copy is left alone");
    assert_eq!(listing(apps.path()), [CURRENT, LEGACY]);
}

#[cfg(unix)]
#[test]
fn even_a_link_with_the_current_name_counts_as_already_there() {
    let apps = tempfile::tempdir().unwrap();
    let old = apps.path().join(LEGACY);
    make_bundle(&old, LEGACY_EXE, "1.3.0");
    std::os::unix::fs::symlink("/nowhere", apps.path().join(CURRENT)).unwrap();
    assert_eq!(install_destination(&old, &names()), old);
}

#[test]
fn a_current_bundle_updates_in_place() {
    let apps = tempfile::tempdir().unwrap();
    let bundle = apps.path().join(CURRENT);
    make_bundle(&bundle, EXE, "1.3.1");
    assert_eq!(install_destination(&bundle, &names()), bundle);
    let installed = install_app(&new_archive("1.3.2"), &bundle, &names(), &no_admin).unwrap();
    assert_eq!(installed.bundle, bundle);
    assert_eq!(version_of(&bundle), "1.3.2");
    assert_eq!(listing(apps.path()), [CURRENT]);
}

#[test]
fn a_bundle_someone_renamed_keeps_its_name() {
    let apps = tempfile::tempdir().unwrap();
    let bundle = apps.path().join("My Example.app");
    make_bundle(&bundle, LEGACY_EXE, "1.3.0");
    assert_eq!(install_destination(&bundle, &names()), bundle);
    let installed = install_app(&new_archive("1.3.1"), &bundle, &names(), &no_admin).unwrap();
    assert_eq!(installed.bundle, bundle);
    assert_eq!(listing(apps.path()), ["My Example.app"]);
}

#[test]
fn a_refused_archive_leaves_the_old_bundle_and_nothing_beside_it() {
    let apps = tempfile::tempdir().unwrap();
    let old = apps.path().join(LEGACY);
    make_bundle(&old, LEGACY_EXE, "1.3.0");
    let no_plist = tar_gz(&[
        ("Method Example.app/", b"", 0o755),
        ("Method Example.app/Contents/MacOS/x", b"x", 0o755),
    ]);
    let err = install_app(&no_plist, &old, &names(), &no_admin).unwrap_err();
    assert!(err.to_string().contains("Info.plist"), "{err}");
    assert_eq!(version_of(&old), "1.3.0");
    assert_eq!(listing(apps.path()), [LEGACY]);
}

#[test]
fn an_info_plist_naming_a_missing_executable_is_refused_before_anything_moves() {
    let apps = tempfile::tempdir().unwrap();
    let old = apps.path().join(CURRENT);
    make_bundle(&old, EXE, "1.3.0");
    let plist = common::info_plist("not-there", "1.3.1");
    let archive = tar_gz(&[
        ("Method Example.app/", b"", 0o755),
        (
            "Method Example.app/Contents/Info.plist",
            plist.as_bytes(),
            0o644,
        ),
        (
            "Method Example.app/Contents/MacOS/method-example",
            b"x",
            0o755,
        ),
    ]);
    let err = install_app(&archive, &old, &names(), &no_admin).unwrap_err();
    assert!(err.to_string().contains("not-there is missing"), "{err}");
    assert_eq!(version_of(&old), "1.3.0");
    assert_eq!(listing(apps.path()), [CURRENT]);
}

#[cfg(unix)]
#[test]
fn an_old_bundle_that_cannot_be_deleted_is_left_over_beside_the_new_one() {
    use std::os::unix::fs::PermissionsExt;
    let apps = tempfile::tempdir().unwrap();
    let old = apps.path().join(LEGACY);
    make_bundle(&old, LEGACY_EXE, "1.3.0");
    let locked = old.join("Contents/Resources/locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(locked.join("f"), "x").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

    let installed = install_app(&new_archive("1.3.1"), &old, &names(), &no_admin).unwrap();
    assert_eq!(installed.bundle, apps.path().join(CURRENT));
    let leftover: std::path::PathBuf = installed.leftover.expect("the old bundle stays behind");
    // Hidden, so the startup sweep finds it and nothing launches it by name.
    assert_eq!(
        leftover.file_name().unwrap().to_string_lossy(),
        format!(".{LEGACY}.old-{}", std::process::id())
    );
    assert!(!old.exists());
    std::fs::set_permissions(
        leftover.join("Contents/Resources/locked"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}

/// The administrator's shell command, run as this account in a scratch
/// folder (the same /bin/sh `do shell script` uses).
#[cfg(unix)]
fn run_admin_shell(bundle: &Path, new: &Path, dest: &Path) -> bool {
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(method_update::install::admin_swap_shell(bundle, new, dest))
        .status()
        .unwrap()
        .success()
}

#[cfg(unix)]
#[test]
fn the_administrator_swap_renames_too() {
    let apps = tempfile::tempdir().unwrap();
    let old = apps.path().join(LEGACY);
    make_bundle(&old, LEGACY_EXE, "1.3.0");
    let staged = tempfile::tempdir().unwrap();
    let new = staged.path().join(CURRENT);
    make_bundle(&new, EXE, "1.3.1");
    let dest = install_destination(&old, &names());
    assert!(run_admin_shell(&old, &new, &dest));
    assert_eq!(version_of(&dest), "1.3.1");
    assert_eq!(listing(apps.path()), [CURRENT]);
}

#[cfg(unix)]
#[test]
fn a_failed_administrator_rename_puts_the_old_bundle_back() {
    let apps = tempfile::tempdir().unwrap();
    let old = apps.path().join(LEGACY);
    make_bundle(&old, LEGACY_EXE, "1.3.0");
    let missing = apps.path().join("not-there.app");
    let dest = install_destination(&old, &names());
    assert!(!run_admin_shell(&old, &missing, &dest));
    assert_eq!(version_of(&old), "1.3.0");
    assert_eq!(listing(apps.path()), [LEGACY]);
}

#[cfg(unix)]
#[test]
fn the_administrator_path_reports_the_renamed_bundle() {
    use std::os::unix::fs::PermissionsExt;
    // A folder this account cannot write: the administrator does the swap
    // (here this account, once the folder is writable again, running the
    // same shell command the real one would).
    let apps = tempfile::tempdir().unwrap();
    let old = apps.path().join(LEGACY);
    make_bundle(&old, LEGACY_EXE, "1.3.0");
    std::fs::set_permissions(apps.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let dir = apps.path().to_path_buf();
    let admin = |bundle: &Path, new: &Path| -> Result<(), InstallError> {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(run_admin_shell(
            bundle,
            new,
            &install_destination(bundle, &names())
        ));
        Ok(())
    };
    let result = install_app(&new_archive("1.3.1"), &old, &names(), &admin);
    std::fs::set_permissions(apps.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let installed = result.unwrap();
    assert_eq!(installed.bundle, apps.path().join(CURRENT));
    assert_eq!(version_of(&installed.bundle), "1.3.1");
    assert_eq!(listing(apps.path()), [CURRENT]);
}

#[cfg(unix)]
#[test]
fn a_declined_administrator_leaves_the_old_bundle() {
    use std::os::unix::fs::PermissionsExt;
    let apps = tempfile::tempdir().unwrap();
    let old = apps.path().join(CURRENT);
    make_bundle(&old, EXE, "1.3.0");
    std::fs::set_permissions(apps.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let declined = |_: &Path, _: &Path| -> Result<(), InstallError> {
        Err(InstallError::NeedsAdmin("declined".into()))
    };
    let result = install_app(&new_archive("1.3.1"), &old, &names(), &declined);
    std::fs::set_permissions(apps.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(result, Err(InstallError::NeedsAdmin(_))));
    assert_eq!(version_of(&old), "1.3.0");
    assert_eq!(listing(apps.path()), [CURRENT]);
}
