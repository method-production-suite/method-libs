//! The startup sweep: a bundle swap that died between its two renames
//! leaves no `Old Example.app`, only the hidden `.Old Example.app.old-<pid>`;
//! the next start puts it back. A swap that finished but could not delete
//! its backup leaves a stale one; the next start removes it, unless it is
//! the bundle this process runs from.

mod common;

use common::{make_bundle, names};
use method_update::install::{Swept, sweep_at_startup, sweep_beside};
use std::path::Path;
use std::time::{Duration, SystemTime};

const ME: u32 = 4_000_000_000;

/// Make the folder at `path` look `by` old. Windows opens a folder only
/// when asked for one (backup semantics), with the right to change its
/// times.
fn age(path: &Path, by: Duration) {
    let mut open = std::fs::OpenOptions::new();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        open.access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    #[cfg(not(windows))]
    open.read(true);
    let f = open.open(path).unwrap();
    f.set_modified(SystemTime::now() - by).unwrap();
}

fn sweep(dir: &Path, name: &str) -> Swept {
    sweep_beside(dir, name, &names(), ME, None)
}

fn version_of(bundle: &Path) -> String {
    std::fs::read_to_string(bundle.join("Contents/Resources/LICENSE")).unwrap()
}

#[test]
fn a_missing_bundle_is_put_back_from_its_hidden_backup() {
    let apps = tempfile::tempdir().unwrap();
    let backup = apps.path().join(".Old Example.app.old-123");
    make_bundle(&backup, "old-example", "0.12.0");
    let staging = apps.path().join(".Old Example.app.update-123");
    std::fs::create_dir_all(staging.join("Contents")).unwrap();
    age(&staging, Duration::from_secs(3600));

    let swept = sweep(apps.path(), "Old Example.app");
    let bundle = apps.path().join("Old Example.app");
    assert_eq!(swept.restored.as_deref(), Some(bundle.as_path()));
    assert_eq!(version_of(&bundle), "0.12.0");
    assert!(!backup.exists());
    // The half-made staging folder goes too, once the bundle is back.
    assert_eq!(swept.removed, std::slice::from_ref(&staging));
    assert!(!staging.exists());
}

#[test]
fn the_newest_runnable_backup_is_the_one_put_back() {
    let apps = tempfile::tempdir().unwrap();
    let older = apps.path().join(".Old Example.app.old-1");
    make_bundle(&older, "old-example", "0.11.0");
    age(&older, Duration::from_secs(7200));
    let newer = apps.path().join(".Old Example.app.old-2");
    make_bundle(&newer, "old-example", "0.12.0");
    age(&newer, Duration::from_secs(3600));
    // Newest of all, but not a bundle anything could run.
    let broken = apps.path().join(".Old Example.app.old-3");
    std::fs::create_dir_all(broken.join("Contents")).unwrap();

    let swept = sweep(apps.path(), "Old Example.app");
    assert!(swept.restored.is_some());
    assert_eq!(version_of(&apps.path().join("Old Example.app")), "0.12.0");
    // The older backup is stale now; the broken one is too new to judge.
    assert_eq!(swept.removed, [older]);
    assert!(broken.exists());
}

#[test]
fn a_stale_backup_beside_a_bundle_is_removed_and_a_fresh_one_kept() {
    let apps = tempfile::tempdir().unwrap();
    let bundle = apps.path().join("Old Example.app");
    make_bundle(&bundle, "old-example", "0.12.0");
    let stale = apps.path().join(".Old Example.app.old-77");
    make_bundle(&stale, "old-example", "0.11.1");
    age(&stale, Duration::from_secs(3600));
    // Minutes old: maybe another copy's swap in progress.
    let fresh = apps.path().join(".Old Example.app.update-78");
    std::fs::create_dir_all(&fresh).unwrap();

    let swept = sweep(apps.path(), "Old Example.app");
    assert_eq!(
        swept,
        Swept {
            restored: None,
            removed: vec![stale.clone()]
        }
    );
    assert!(!stale.exists());
    assert!(fresh.exists());
    assert_eq!(version_of(&bundle), "0.12.0", "the bundle is never touched");
}

#[test]
fn this_processes_own_swap_and_anything_else_are_left_alone() {
    let apps = tempfile::tempdir().unwrap();
    make_bundle(
        &apps.path().join("Old Example.app"),
        "old-example",
        "0.12.0",
    );
    let mine = apps.path().join(format!(".Old Example.app.old-{ME}"));
    std::fs::create_dir_all(&mine).unwrap();
    let other_app = apps.path().join(".Other.app.old-5");
    std::fs::create_dir_all(&other_app).unwrap();
    let not_a_pid = apps.path().join(".Old Example.app.old-x");
    std::fs::create_dir_all(&not_a_pid).unwrap();
    let a_file = apps.path().join(".Old Example.app.old-6");
    std::fs::write(&a_file, "not a folder").unwrap();
    for p in [&mine, &other_app, &not_a_pid] {
        age(p, Duration::from_secs(3600));
    }
    let swept = sweep(apps.path(), "Old Example.app");
    assert_eq!(swept, Swept::default());
    for p in [&mine, &other_app, &not_a_pid, &a_file] {
        assert!(p.exists(), "{}", p.display());
    }
}

#[cfg(unix)]
#[test]
fn a_link_named_like_a_backup_is_never_followed_or_restored() {
    let apps = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    make_bundle(&elsewhere.path().join("Evil.app"), "old-example", "6.6.6");
    std::os::unix::fs::symlink(
        elsewhere.path().join("Evil.app"),
        apps.path().join(".Old Example.app.old-9"),
    )
    .unwrap();
    let swept = sweep(apps.path(), "Old Example.app");
    assert_eq!(swept, Swept::default());
    assert!(!apps.path().join("Old Example.app").exists());
    assert!(elsewhere.path().join("Evil.app").exists());
}

#[test]
fn no_bundle_and_no_usable_backup_touches_nothing() {
    let apps = tempfile::tempdir().unwrap();
    let staging = apps.path().join(".Old Example.app.update-12");
    std::fs::create_dir_all(&staging).unwrap();
    age(&staging, Duration::from_secs(3600));
    assert_eq!(sweep(apps.path(), "Old Example.app"), Swept::default());
    assert!(staging.exists(), "nothing to fall back on: evidence kept");
}

#[test]
fn started_from_the_hidden_backup_the_app_puts_itself_back() {
    let apps = tempfile::tempdir().unwrap();
    let backup = apps.path().join(".Old Example.app.old-123");
    make_bundle(&backup, "old-example", "0.12.0");
    let exe = backup.join("Contents/MacOS/old-example");
    let swept = sweep_at_startup(&exe, &names());
    assert_eq!(
        swept.restored.as_deref(),
        Some(apps.path().join("Old Example.app").as_path())
    );
    assert!(!backup.exists());
}

#[test]
fn started_from_the_bundle_the_app_clears_its_stale_backups() {
    let apps = tempfile::tempdir().unwrap();
    let bundle = apps.path().join("Old Example.app");
    make_bundle(&bundle, "old-example", "0.12.0");
    let stale = apps.path().join(".Old Example.app.old-123");
    std::fs::create_dir_all(&stale).unwrap();
    age(&stale, Duration::from_secs(3600));
    let swept = sweep_at_startup(&bundle.join("Contents/MacOS/old-example"), &names());
    assert_eq!(swept.removed, [stale]);
}

#[test]
fn outside_a_bundle_the_sweep_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let stale = dir.path().join(".Old Example.app.old-123");
    std::fs::create_dir_all(&stale).unwrap();
    age(&stale, Duration::from_secs(3600));
    for exe in [
        dir.path().join("method-example"),
        dir.path().join("target/release/method-example"),
        dir.path()
            .join(".Old Example.app.update-5/Contents/MacOS/old-example"),
    ] {
        assert_eq!(
            sweep_at_startup(&exe, &names()),
            Swept::default(),
            "{}",
            exe.display()
        );
    }
    assert!(stale.exists());
}

// ---- the one-time rename: leftovers under either name ----
//
// A renaming swap moves `Old Example.app` aside to `.Old Example.app.old-<pid>`
// and its staging `.Method Example.app.update-<pid>` into
// `Method Example.app`. A sweep beside either name looks after both.

#[test]
fn a_renaming_swap_that_died_between_its_renames_puts_the_old_bundle_back() {
    let apps = tempfile::tempdir().unwrap();
    let backup = apps.path().join(".Old Example.app.old-123");
    make_bundle(&backup, "old-example", "0.13.0");
    age(&backup, Duration::from_secs(3600));
    let staging = apps.path().join(".Method Example.app.update-123");
    std::fs::create_dir_all(staging.join("Contents")).unwrap();
    age(&staging, Duration::from_secs(3600));

    // Launch Services follows the moved bundle: the Dock starts the backup.
    let swept = sweep_at_startup(&backup.join("Contents/MacOS/old-example"), &names());
    let bundle = apps.path().join("Old Example.app");
    assert_eq!(swept.restored.as_deref(), Some(bundle.as_path()));
    assert_eq!(version_of(&bundle), "0.13.0");
    assert_eq!(swept.removed, std::slice::from_ref(&staging));
    assert!(!apps.path().join("Method Example.app").exists());
}

#[test]
fn the_new_bundle_clears_the_old_name_s_stale_backup() {
    let apps = tempfile::tempdir().unwrap();
    let bundle = apps.path().join("Method Example.app");
    make_bundle(&bundle, "method-example", "0.13.1");
    let stale = apps.path().join(".Old Example.app.old-77");
    make_bundle(&stale, "old-example", "0.13.0");
    age(&stale, Duration::from_secs(3600));
    let fresh = apps.path().join(".Old Example.app.old-78");
    std::fs::create_dir_all(&fresh).unwrap();

    let swept = sweep_at_startup(&bundle.join("Contents/MacOS/method-example"), &names());
    assert_eq!(
        swept,
        Swept {
            restored: None,
            removed: vec![stale.clone()]
        }
    );
    assert!(fresh.exists(), "minutes old: maybe a swap in progress");
    assert!(!apps.path().join("Old Example.app").exists());
}

#[test]
fn started_from_the_old_backup_beside_the_new_bundle_nothing_is_put_back() {
    // The rename finished but could not delete the backup, and the Dock
    // still points at the moved old bundle. Putting it back would leave two
    // copies; the new bundle is already where it belongs.
    let apps = tempfile::tempdir().unwrap();
    make_bundle(
        &apps.path().join("Method Example.app"),
        "method-example",
        "0.13.1",
    );
    let backup = apps.path().join(".Old Example.app.old-123");
    make_bundle(&backup, "old-example", "0.13.0");
    let swept = sweep_at_startup(&backup.join("Contents/MacOS/old-example"), &names());
    assert_eq!(swept.restored, None);
    assert!(!apps.path().join("Old Example.app").exists());
}

#[test]
fn the_old_bundle_clears_a_renaming_swap_s_stale_staging() {
    let apps = tempfile::tempdir().unwrap();
    let bundle = apps.path().join("Old Example.app");
    make_bundle(&bundle, "old-example", "0.13.0");
    let staging = apps.path().join(".Method Example.app.update-5");
    std::fs::create_dir_all(staging.join("Contents")).unwrap();
    age(&staging, Duration::from_secs(3600));
    let swept = sweep(apps.path(), "Old Example.app");
    assert_eq!(swept.removed, [staging]);
    assert_eq!(swept.restored, None);
}

#[test]
fn a_missing_new_bundle_is_put_back_from_its_own_backup() {
    let apps = tempfile::tempdir().unwrap();
    let backup = apps.path().join(".Method Example.app.old-9");
    make_bundle(&backup, "method-example", "0.13.1");
    let swept = sweep(apps.path(), "Method Example.app");
    let bundle = apps.path().join("Method Example.app");
    assert_eq!(swept.restored.as_deref(), Some(bundle.as_path()));
    assert!(!apps.path().join("Old Example.app").exists());
}

// ---- launched from a stale backup ----

#[test]
fn launched_from_a_stale_backup_beside_the_bundle_it_is_never_deleted() {
    // The Dock still points at a backup the swap could not delete, and it
    // is older than the ten minutes after which leftovers go. Deleting it
    // would delete the running app out from under itself.
    let apps = tempfile::tempdir().unwrap();
    make_bundle(
        &apps.path().join("Method Example.app"),
        "method-example",
        "1.3.1",
    );
    let running = apps.path().join(".Method Example.app.old-123");
    make_bundle(&running, "method-example", "1.3.0");
    age(&running, Duration::from_secs(3600));
    let other = apps.path().join(".Method Example.app.old-124");
    std::fs::create_dir_all(&other).unwrap();
    age(&other, Duration::from_secs(3600));

    let swept = sweep_at_startup(&running.join("Contents/MacOS/method-example"), &names());
    assert_eq!(swept.restored, None);
    assert_eq!(swept.removed, [other], "other stale leftovers still go");
    assert!(running.join("Contents/MacOS/method-example").is_file());
}

#[test]
fn launched_from_a_stale_legacy_backup_beside_the_renamed_bundle_it_is_kept() {
    let apps = tempfile::tempdir().unwrap();
    make_bundle(
        &apps.path().join("Method Example.app"),
        "method-example",
        "1.3.1",
    );
    let running = apps.path().join(".Old Example.app.old-77");
    make_bundle(&running, "old-example", "1.3.0");
    age(&running, Duration::from_secs(3600));
    let swept = sweep_at_startup(&running.join("Contents/MacOS/old-example"), &names());
    assert_eq!(swept, Swept::default());
    assert!(running.exists());
}

#[test]
fn launched_from_one_backup_while_a_newer_one_is_put_back_the_running_one_stays() {
    let apps = tempfile::tempdir().unwrap();
    let running = apps.path().join(".Method Example.app.old-1");
    make_bundle(&running, "method-example", "1.2.0");
    age(&running, Duration::from_secs(7200));
    let newer = apps.path().join(".Method Example.app.old-2");
    make_bundle(&newer, "method-example", "1.3.0");
    age(&newer, Duration::from_secs(3600));
    let swept = sweep_at_startup(&running.join("Contents/MacOS/method-example"), &names());
    assert_eq!(
        swept.restored.as_deref(),
        Some(apps.path().join("Method Example.app").as_path())
    );
    assert_eq!(version_of(&apps.path().join("Method Example.app")), "1.3.0");
    assert!(swept.removed.is_empty());
    assert!(running.exists(), "the running bundle is never deleted");
}

#[test]
fn a_bundle_with_no_legacy_name_sweeps_only_its_own_leftovers() {
    let apps = tempfile::tempdir().unwrap();
    make_bundle(
        &apps.path().join("Method Example.app"),
        "method-example",
        "1.3.1",
    );
    let legacy = apps.path().join(".Old Example.app.old-5");
    std::fs::create_dir_all(&legacy).unwrap();
    age(&legacy, Duration::from_secs(3600));
    let swept = sweep_beside(
        apps.path(),
        "Method Example.app",
        &method_update::BundleNames::new("Method Example.app"),
        ME,
        None,
    );
    assert_eq!(swept, Swept::default());
    assert!(legacy.exists());
}
