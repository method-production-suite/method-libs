//! What the bundle swap does with an archive that is signed but strange,
//! and where the administrator path stages its copy.
//!
//! Every archive reaches the unpacker only after its signature checks, so
//! these are defence in depth: a link entry that points out of the bundle,
//! followed by a file "under" it, would otherwise be written wherever the
//! link points (tar's `unpack` follows links and resolves a hard link's
//! name against the process's working folder).

#![cfg(unix)]

mod common;

use common::{archive_bundle, make_bundle, names};
use method_update::install::{InstallError, install_app, unpack_app_archive};
use std::path::Path;

/// A bundle archive with `extra` (path, kind, link target or data) appended.
fn archive_with(extra: &[(&str, tar::EntryType, &str)]) -> Vec<u8> {
    let build = tempfile::tempdir().unwrap();
    make_bundle(&build.path().join("Built.app"), "method-example", "1.3.0");
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut b = tar::Builder::new(gz);
    b.append_dir_all("Method Example.app", build.path().join("Built.app"))
        .unwrap();
    for (path, kind, what) in extra {
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o755);
        h.set_entry_type(*kind);
        match kind {
            tar::EntryType::Symlink | tar::EntryType::Link => {
                h.set_size(0);
                b.append_link(&mut h, path, what).unwrap();
            }
            _ => {
                h.set_size(what.len() as u64);
                b.append_data(&mut h, path, what.as_bytes()).unwrap();
            }
        }
    }
    b.into_inner().unwrap().finish().unwrap()
}

fn no_admin(_: &Path, _: &Path) -> Result<(), InstallError> {
    panic!("no administrator should be needed in a temp folder")
}

#[test]
fn a_link_out_of_the_bundle_is_refused_and_nothing_lands_outside() {
    let outside = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let bytes = archive_with(&[
        (
            "Method Example.app/Contents/Escape",
            tar::EntryType::Symlink,
            outside.path().to_str().unwrap(),
        ),
        (
            "Method Example.app/Contents/Escape/planted",
            tar::EntryType::Regular,
            "x",
        ),
    ]);
    let err = unpack_app_archive(&bytes, &dest.path().join("staged")).unwrap_err();
    assert!(err.to_string().contains("outside its bundle"), "{err}");
    assert!(!outside.path().join("planted").exists());

    let climbing = archive_with(&[(
        "Method Example.app/Contents/Up",
        tar::EntryType::Symlink,
        "../../../..",
    )]);
    let err = unpack_app_archive(&climbing, &dest.path().join("staged2")).unwrap_err();
    assert!(err.to_string().contains("outside its bundle"), "{err}");
}

#[test]
fn chains_of_links_cannot_climb_out_either() {
    let dest = tempfile::tempdir().unwrap();
    // A file written through a link already unpacked.
    let through = archive_with(&[
        (
            "Method Example.app/Contents/L",
            tar::EntryType::Symlink,
            ".",
        ),
        (
            "Method Example.app/Contents/L/L/x",
            tar::EntryType::Regular,
            "x",
        ),
    ]);
    assert!(unpack_app_archive(&through, &dest.path().join("a")).is_err());
    // A `..` taken from where another link points.
    let after = archive_with(&[
        (
            "Method Example.app/Contents/L",
            tar::EntryType::Symlink,
            "..",
        ),
        (
            "Method Example.app/Contents/X",
            tar::EntryType::Symlink,
            "L/../..",
        ),
    ]);
    assert!(unpack_app_archive(&after, &dest.path().join("b")).is_err());
}

#[test]
fn hard_links_and_devices_are_refused() {
    let dest = tempfile::tempdir().unwrap();
    let hard = archive_with(&[(
        "Method Example.app/Contents/Hard",
        tar::EntryType::Link,
        "/etc/hosts",
    )]);
    assert!(unpack_app_archive(&hard, &dest.path().join("a")).is_err());
    let fifo = archive_with(&[("Method Example.app/Contents/Pipe", tar::EntryType::Fifo, "")]);
    assert!(unpack_app_archive(&fifo, &dest.path().join("b")).is_err());
}

#[test]
fn a_link_that_stays_inside_the_bundle_is_kept() {
    // A framework's `Versions/Current -> A` is how a bundle with
    // frameworks is laid out; the release pipelines keep symlinks as
    // symlinks.
    let dest = tempfile::tempdir().unwrap();
    let bytes = archive_with(&[(
        "Method Example.app/Contents/Current",
        tar::EntryType::Symlink,
        "Resources",
    )]);
    let staged = dest.path().join("staged");
    unpack_app_archive(&bytes, &staged).unwrap();
    let link = staged.join("Contents/Current");
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert!(link.join("LICENSE").is_file());
}

/// The administrator's shell command, run as this account in a scratch
/// folder (the same /bin/sh `do shell script` uses).
fn run_admin_shell(bundle: &Path, new: &Path) -> bool {
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(method_update::install::admin_swap_shell(
            bundle, new, bundle,
        ))
        .status()
        .unwrap()
        .success()
}

#[test]
fn the_administrator_swap_puts_the_old_app_back_when_the_move_fails() {
    // `rm -rf old && mv -f new old`: a move that failed after the remove
    // (another volume, a full disk) left no app at all.
    let apps = tempfile::tempdir().unwrap();
    let bundle = apps.path().join("Method Example.app");
    make_bundle(&bundle, "method-example", "1.3.0");
    let missing = apps.path().join("not-there.app");
    assert!(
        !run_admin_shell(&bundle, &missing),
        "the swap reports failure"
    );
    assert!(
        bundle.join("Contents/Info.plist").is_file(),
        "the old app is still there"
    );
    let names: Vec<_> = std::fs::read_dir(apps.path()).unwrap().collect();
    assert_eq!(names.len(), 1, "nothing left beside it");
}

#[test]
fn the_administrator_swap_replaces_the_app_and_leaves_nothing_beside_it() {
    let apps = tempfile::tempdir().unwrap();
    let bundle = apps.path().join("Ann's \"Method Example\".app");
    make_bundle(&bundle, "method-example", "1.3.0");
    let staged = tempfile::tempdir().unwrap();
    let new = staged.path().join("Method Example.app");
    make_bundle(&new, "method-example", "1.3.1");
    assert!(run_admin_shell(&bundle, &new));
    assert!(bundle.join("Contents/MacOS/method-example").is_file());
    let names: Vec<_> = std::fs::read_dir(apps.path()).unwrap().collect();
    assert_eq!(names.len(), 1, "no backup left beside it");
    // And with no app there yet (the first install).
    let fresh = apps.path().join("Fresh.app");
    let new2 = staged.path().join("Two.app");
    make_bundle(&new2, "method-example", "1.3.1");
    assert!(run_admin_shell(&fresh, &new2));
    assert!(fresh.join("Contents/MacOS/method-example").is_file());
}

#[test]
fn the_administrator_path_stages_in_a_fresh_private_folder() {
    use std::os::unix::fs::PermissionsExt;
    // A bundle in a folder this account cannot write sends the swap to
    // the administrator path. Its copy must never sit at a predictable
    // temp/<name>-<pid>, where a folder made first by another account
    // would be used as found.
    let apps = tempfile::tempdir().unwrap();
    let bundle = apps.path().join("Method Example.app");
    make_bundle(&bundle, "method-example", "1.3.0");
    let archive = {
        let build = tempfile::tempdir().unwrap();
        make_bundle(&build.path().join("Built.app"), "method-example", "1.3.0");
        archive_bundle(&build.path().join("Built.app"))
    };
    std::fs::set_permissions(apps.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let predictable = format!("method-update-{}", std::process::id());
    let seen = std::cell::RefCell::new(None);
    let admin = |_: &Path, new: &Path| -> Result<(), InstallError> {
        let mode = std::fs::metadata(new.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        *seen.borrow_mut() = Some((new.to_path_buf(), mode & 0o777));
        Ok(())
    };
    let result = install_app(&archive, &bundle, &names(), &admin);
    std::fs::set_permissions(apps.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    result.unwrap();
    let (new, mode) = seen.into_inner().expect("the administrator was asked");
    assert!(
        !new.ancestors()
            .any(|a| a.file_name() == Some(std::ffi::OsStr::new(&predictable))),
        "{}",
        new.display()
    );
    assert_eq!(mode, 0o700, "only this account can reach the staged copy");
    assert!(!new.exists(), "the staged copy is cleaned up");
    let _ = no_admin;
}

#[test]
fn an_absolute_path_or_a_climb_is_refused() {
    let dest = tempfile::tempdir().unwrap();
    for name in ["/etc/planted", "Method Example.app/../../planted"] {
        let bytes = common::tar_gz(&[(name, b"x", 0o644)]);
        let err = unpack_app_archive(&bytes, &dest.path().join("staged")).unwrap_err();
        assert!(
            err.to_string().contains("outside its bundle"),
            "{name}: {err}"
        );
    }
    assert!(!dest.path().join("planted").exists());
}
