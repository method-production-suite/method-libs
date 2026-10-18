//! Putting a verified update in place: the Mac bundle swap, made in the
//! shape of the manifest format's own updater (tauri-plugin-updater
//! 2.12.0): unpack the `.app.tar.gz` with its top folder dropped (so the
//! archive's own `.app` name never matters), move the running bundle aside,
//! move the new one into the SAME path, touch it. Stricter than that
//! updater, in ways no valid archive can notice:
//!
//! - the staging and backup folders sit BESIDE the bundle, not in the
//!   system temp folder, so both renames stay on one volume (a rename into
//!   the temp folder fails for an app kept on another disk);
//! - an entry that would land outside the bundle (`..`, an absolute path, a
//!   link out of it) is refused, and the unpacked bundle must carry an
//!   Info.plist whose CFBundleExecutable exists before anything is moved;
//! - a failure after the running bundle moved puts it back.
//!
//! A backup that cannot be deleted afterwards is reported as
//! [`Installed::leftover`], not as a failure: the new bundle is in place.
//! When /Applications is not writable, an administrator does the swap
//! through osascript ([`admin_swap`]), with the paths quoted for the shell
//! and for AppleScript.
//!
//! One swap renames as well, for an app that has a legacy bundle name
//! ([`BundleNames::legacy`]). When the running bundle has that legacy name
//! and nothing with the current name sits beside it, the new version goes
//! in under the current name: the old bundle moves aside to its hidden
//! backup as always, the new one moves in under the new name, the backup
//! is removed, and [`Installed::bundle`] names the new path for the
//! restart. A failure puts the old bundle back under its old name. Nothing
//! else ever renames a bundle.
//!
//! [`sweep_at_startup`] cleans up after a swap that was interrupted. The
//! updater never reads or checks the bundle identifier: the signature is
//! what vouches for an update.

use std::io::Cursor;
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;

use crate::config::BundleNames;

#[derive(Debug)]
pub enum InstallError {
    Io(String),
    /// The archive is not a bundle the app would run.
    Archive(String),
    /// The bundle's folder is not writable and the person declined (or no
    /// administrator could be asked).
    NeedsAdmin(String),
    /// No downloaded update is waiting to be installed.
    NothingReady,
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(s) | Self::Archive(s) | Self::NeedsAdmin(s) => f.write_str(s),
            Self::NothingReady => f.write_str("No update is ready to install."),
        }
    }
}

impl std::error::Error for InstallError {}

fn io(what: &str, path: &Path, e: std::io::Error) -> InstallError {
    InstallError::Io(format!("Could not {what} {}: {e}", path.display()))
}

/// Where an update of `bundle` goes: the current name beside it when
/// `bundle` has the LEGACY name and nothing (not even a link) has the
/// current name yet, else `bundle` itself. A bundle someone renamed by hand
/// keeps its name.
pub fn install_destination(bundle: &Path, names: &BundleNames) -> PathBuf {
    let renamed = bundle.with_file_name(&names.current);
    let is_legacy = names
        .legacy
        .as_deref()
        .is_some_and(|l| bundle.file_name() == Some(std::ffi::OsStr::new(l)));
    if is_legacy && std::fs::symlink_metadata(&renamed).is_err() {
        renamed
    } else {
        bundle.to_path_buf()
    }
}

/// What an install left behind.
#[derive(Debug, PartialEq, Eq)]
pub struct Installed {
    /// Where the new version now is: the bundle's own path, or the current
    /// name beside it after the one-time rename. The restart starts this
    /// path.
    pub bundle: PathBuf,
    /// The old bundle, when deleting it failed. The update still counts.
    pub leftover: Option<PathBuf>,
}

/// The `.app` a running executable lives in
/// (`/Applications/Method Example.app/Contents/MacOS/method-example` gives
/// `/Applications/Method Example.app`), or None outside a bundle, which is
/// how a development build knows it cannot update itself.
pub fn bundle_of_exe(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app")
        .then(|| bundle.to_path_buf())
}

/// Unpack a `.app.tar.gz` into `dest`, dropping the first component of
/// every entry as the format's updater does.
pub fn unpack_app_archive(bytes: &[u8], dest: &Path) -> Result<(), InstallError> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Err(InstallError::Archive(
            "The update is not a gzip archive.".into(),
        ));
    }
    std::fs::create_dir_all(dest).map_err(|e| io("create", dest, e))?;
    let mut archive = tar::Archive::new(GzDecoder::new(Cursor::new(bytes)));
    let entries = archive
        .entries()
        .map_err(|e| InstallError::Archive(format!("The update archive is unreadable: {e}")))?;
    for entry in entries {
        let mut entry = entry
            .map_err(|e| InstallError::Archive(format!("The update archive is unreadable: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| InstallError::Archive(format!("A name in the update is unreadable: {e}")))?
            .into_owned();
        let mut rest = PathBuf::new();
        for c in path.components().skip(1) {
            match c {
                Component::Normal(p) => rest.push(p),
                Component::CurDir => {}
                _ => {
                    return Err(InstallError::Archive(format!(
                        "The update names a file outside its bundle: {}",
                        path.display()
                    )));
                }
            }
        }
        if path
            .components()
            .next()
            .is_some_and(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(InstallError::Archive(format!(
                "The update names a file outside its bundle: {}",
                path.display()
            )));
        }
        let target = dest.join(&rest);
        if rest.as_os_str().is_empty() {
            continue;
        }
        // Files, folders, and links that stay inside the bundle. A link out
        // of it, with a file "under" it later in the archive, would be
        // written through (tar's unpack follows links), and a hard link's
        // name resolves against the working folder, not the bundle. A
        // signature already vouches for the archive, so this only refuses
        // what a bundle never holds.
        match entry.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Directory => {}
            tar::EntryType::Symlink => {
                let link = entry
                    .link_name()
                    .ok()
                    .flatten()
                    .map(|l| l.into_owned())
                    .unwrap_or_default();
                if !link_stays_inside(&rest, &link) {
                    return Err(InstallError::Archive(format!(
                        "The update names a file outside its bundle: {} -> {}",
                        path.display(),
                        link.display()
                    )));
                }
            }
            other => {
                return Err(InstallError::Archive(format!(
                    "The update holds an entry a bundle never has ({other:?}): {}",
                    path.display()
                )));
            }
        }
        // Nothing is written THROUGH a link already unpacked: the lexical
        // check above cannot see a chain of links that loop back on
        // themselves (`L -> .`, then `L/L/L/x -> ../../..`).
        let mut walked = dest.to_path_buf();
        for c in rest.parent().into_iter().flat_map(Path::components) {
            walked.push(c);
            if walked
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                return Err(InstallError::Archive(format!(
                    "The update names a file outside its bundle: {}",
                    path.display()
                )));
            }
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io("create", parent, e))?;
        }
        entry
            .unpack(&target)
            .map_err(|e| io("unpack", &target, e))?;
    }
    Ok(())
}

/// Whether a symlink at `at` (relative to the bundle) pointing at `link`
/// resolves inside the bundle: relative, its `..`s all leading and never
/// climbing above the root, then only names. A `..` after a name is
/// refused, because that name may itself be a link and `..` is taken from
/// where it points (`L -> ..`, then `X -> L/../..`). Every link checked
/// this way resolves inside, so descending through one stays inside.
fn link_stays_inside(at: &Path, link: &Path) -> bool {
    let mut depth = at.components().count().saturating_sub(1) as i64;
    if link.as_os_str().is_empty() {
        return false;
    }
    let mut named = false;
    for c in link.components() {
        match c {
            Component::Normal(_) => named = true,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if named || depth < 0 {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// The executable an unpacked (or installed) bundle names, checked to
/// exist: what the restart reads (`CFBundleExecutable`).
pub fn bundle_executable(bundle: &Path) -> Result<PathBuf, InstallError> {
    let info = bundle.join("Contents").join("Info.plist");
    let dict: plist::Dictionary = plist::from_file(&info).map_err(|e| {
        InstallError::Archive(format!("The update has no readable Info.plist: {e}"))
    })?;
    let name = dict
        .get("CFBundleExecutable")
        .and_then(plist::Value::as_string)
        .ok_or_else(|| {
            InstallError::Archive("The update's Info.plist names no executable.".into())
        })?;
    let exe = bundle.join("Contents").join("MacOS").join(name);
    if !exe.is_file() {
        return Err(InstallError::Archive(format!(
            "The update's executable {name} is missing."
        )));
    }
    Ok(exe)
}

/// The bundle's `CFBundleIdentifier`, if it has one. Only for an app that
/// wants to know where its data lives; the updater never checks it.
pub fn bundle_identifier(bundle: &Path) -> Option<String> {
    let dict: plist::Dictionary =
        plist::from_file(bundle.join("Contents").join("Info.plist")).ok()?;
    dict.get("CFBundleIdentifier")?
        .as_string()
        .map(str::to_string)
}

fn sibling(bundle: &Path, tag: &str) -> PathBuf {
    let name = bundle
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    bundle.with_file_name(format!(".{name}.{tag}-{}", std::process::id()))
}

/// A hidden sibling's name taken apart: `.Method Example.app.old-123` is
/// `("Method Example.app", "old", 123)`, and `.update-` the same.
fn parse_sibling(file_name: &str) -> Option<(&str, &str, u32)> {
    let rest = file_name.strip_prefix('.')?;
    let (head, pid) = rest.rsplit_once('-')?;
    let pid: u32 = pid.parse().ok()?;
    let (name, tag) = head.rsplit_once('.')?;
    (matches!(tag, "old" | "update") && name.ends_with(".app") && name.len() > 4)
        .then_some((name, tag, pid))
}

/// What the startup sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// The bundle put back from its hidden backup.
    pub restored: Option<PathBuf>,
    /// Leftovers removed.
    pub removed: Vec<PathBuf>,
}

/// A leftover younger than this may belong to a swap still running in
/// another copy of the app, so it is left alone (a swap takes seconds).
const LEFTOVER_AGE: std::time::Duration = std::time::Duration::from_secs(600);

/// The leftovers of an interrupted swap beside `dir/name`
/// (`.<name>.old-<pid>` and `.<name>.update-<pid>`, from any process but
/// `own_pid`; for the app's current and legacy names, those of both). A
/// swap that died between its two renames leaves no bundle at all, only
/// the hidden backup: then the newest backup that still holds a runnable
/// bundle is moved back under the name it had. Once a bundle is there,
/// leftovers older than ten minutes are removed, except `running`: the
/// bundle this process runs from is never deleted, however old. With no
/// bundle and no usable backup nothing is touched.
pub fn sweep_beside(
    dir: &Path,
    name: &str,
    bundle_names: &BundleNames,
    own_pid: u32,
    running: Option<&Path>,
) -> Swept {
    let mut swept = Swept::default();
    let names = bundle_names.standing_for(name);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return swept;
    };
    let mut leftovers: Vec<(PathBuf, String, bool, std::time::SystemTime)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let file = e.file_name();
            let (n, tag, pid) = parse_sibling(file.to_str()?)?;
            // A real folder only: never follow a link out of the folder.
            let meta = std::fs::symlink_metadata(e.path()).ok()?;
            (names.contains(&n) && pid != own_pid && meta.is_dir()).then(|| {
                (
                    e.path(),
                    n.to_string(),
                    tag == "old",
                    meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                )
            })
        })
        .collect();
    // Newest first.
    leftovers.sort_by_key(|l| std::cmp::Reverse(l.3));
    let present = names
        .iter()
        .any(|n| std::fs::symlink_metadata(dir.join(n)).is_ok());
    if !present {
        let back = leftovers.iter().position(|(p, n, old, _)| {
            *old && bundle_executable(p).is_ok() && std::fs::rename(p, dir.join(n)).is_ok()
        });
        match back {
            Some(i) => {
                let (_, n, _, _) = leftovers.remove(i);
                let bundle = dir.join(n);
                touch(&bundle);
                swept.restored = Some(bundle);
            }
            None => return swept,
        }
    }
    let now = std::time::SystemTime::now();
    for (path, _, _, modified) in leftovers {
        if running.is_some_and(|r| same_place(r, &path)) {
            continue;
        }
        let age = now.duration_since(modified).unwrap_or_default();
        if age >= LEFTOVER_AGE && std::fs::remove_dir_all(&path).is_ok() {
            swept.removed.push(path);
        }
    }
    swept
}

/// Whether two paths name the same folder: equal, or equal once both are
/// made canonical.
fn same_place(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The sweep an app runs as it starts: from its own executable, the bundle
/// it runs in, and the name that bundle should have. Started from a hidden
/// backup (Launch Services follows a moved bundle, so the Dock can open
/// `.Method Example.app.old-<pid>` when `Method Example.app` is gone), the
/// backup is the one put back, unless a bundle with its name (or the other
/// of the app's two names) is already there; then the backup it runs from
/// is left alone, however old, and a later start removes it. Outside a
/// bundle (a development build, Windows, Linux) nothing is done.
pub fn sweep_at_startup(exe: &Path, names: &BundleNames) -> Swept {
    let bundle = (|| {
        let macos = exe.parent()?;
        let contents = macos.parent()?;
        (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents")
            .then(|| contents.parent())?
    })();
    let Some(bundle) = bundle else {
        return Swept::default();
    };
    let (Some(dir), Some(file)) = (bundle.parent(), bundle.file_name().and_then(|f| f.to_str()))
    else {
        return Swept::default();
    };
    let name = match parse_sibling(file) {
        Some((name, "old", _)) => name,
        Some(_) => return Swept::default(),
        None if file.ends_with(".app") => file,
        None => return Swept::default(),
    };
    sweep_beside(dir, name, names, std::process::id(), Some(bundle))
}

fn permission(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied
}

/// What asks an administrator to swap `bundle` (first argument) for the
/// staged copy (second): [`admin_swap`] on a Mac.
pub type Admin<'a> = &'a dyn Fn(&Path, &Path) -> Result<(), InstallError>;

/// Swap `bundle` for the one in `bytes`, at [`install_destination`]: the
/// same path, or the current name beside a bundle with the legacy name. A
/// folder the person cannot write (an /Applications owned by another
/// account) needs an administrator: `admin` is asked to run the swap of
/// `bundle` for the staged copy (on a Mac, [`admin_swap`] through
/// osascript).
pub fn install_app(
    bytes: &[u8],
    bundle: &Path,
    names: &BundleNames,
    admin: Admin<'_>,
) -> Result<Installed, InstallError> {
    let dest = install_destination(bundle, names);
    let staging = sibling(&dest, "update");
    let _ = std::fs::remove_dir_all(&staging);
    match std::fs::create_dir(&staging) {
        Ok(()) => {}
        Err(e) if permission(&e) => return install_with_admin(bytes, bundle, names, admin),
        Err(e) => return Err(io("create", &staging, e)),
    }
    let staged = (|| {
        unpack_app_archive(bytes, &staging)?;
        bundle_executable(&staging)
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    let backup = sibling(bundle, "old");
    let _ = std::fs::remove_dir_all(&backup);
    let had_old = bundle.exists();
    if had_old && let Err(e) = std::fs::rename(bundle, &backup) {
        let _ = std::fs::remove_dir_all(&staging);
        if permission(&e) {
            return install_with_admin(bytes, bundle, names, admin);
        }
        return Err(io("move aside", bundle, e));
    }
    if let Err(e) = std::fs::rename(&staging, &dest) {
        if had_old {
            let _ = std::fs::rename(&backup, bundle);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(io("move the new bundle into", &dest, e));
    }
    let leftover = (had_old && std::fs::remove_dir_all(&backup).is_err()).then_some(backup);
    touch(&dest);
    Ok(Installed {
        bundle: dest,
        leftover,
    })
}

fn install_with_admin(
    bytes: &[u8],
    bundle: &Path,
    names: &BundleNames,
    admin: Admin<'_>,
) -> Result<Installed, InstallError> {
    let dest = install_destination(bundle, names);
    // The copy an administrator's shell moves into place is staged in a
    // folder made fresh for it, readable by this account alone: a
    // predictable name in a shared temp folder could be made first by
    // another account, and its contents swapped between the check below
    // and the root `mv`.
    let private = private_temp_dir()?;
    let temp = private.join(&names.current);
    let result = (|| {
        unpack_app_archive(bytes, &temp)?;
        bundle_executable(&temp)?;
        admin(bundle, &temp)
    })();
    let _ = std::fs::remove_dir_all(&private);
    result?;
    // [`admin_swap`] renames exactly when `dest` differs; what is on disk
    // says which happened.
    let installed = if dest != bundle && dest.exists() && !bundle.exists() {
        dest
    } else {
        bundle.to_path_buf()
    };
    touch(&installed);
    Ok(Installed {
        bundle: installed,
        leftover: None,
    })
}

/// A new folder in the temp folder that did not exist before this call,
/// mode 0700 where the platform has modes.
fn private_temp_dir() -> Result<PathBuf, InstallError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir();
    for _ in 0..64 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let name = format!(
            "method-update-{}-{}-{nanos:08x}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let dir = base.join(name);
        // Private to this user on unix; Windows has no mode bits, and a
        // folder under the user's own temp folder is already theirs.
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut b = std::fs::DirBuilder::new();
            b.mode(0o700);
            b
        };
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        // create, not create_all: an existing folder is never reused.
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io("create", &dir, e)),
        }
    }
    Err(InstallError::Archive(
        "Could not make a private folder to stage the update in.".into(),
    ))
}

/// Bump the bundle's modification time, so Finder and Launch Services read
/// the new Info.plist.
pub fn touch(path: &Path) {
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
}

/// The shell command [`admin_swap_script`] hands an administrator: the old
/// app moved aside, the new one moved in at `dest` (the old app's own path,
/// or the current name beside it: [`install_destination`]), and the old one
/// put back if that move fails (removing it first would leave no app at all
/// when the move failed after it), then the old one removed.
pub fn admin_swap_shell(bundle: &Path, new: &Path, dest: &Path) -> String {
    let src = shell_quote(&bundle.to_string_lossy());
    let bak = shell_quote(&sibling(bundle, "old").to_string_lossy());
    let new = shell_quote(&new.to_string_lossy());
    let dst = shell_quote(&dest.to_string_lossy());
    format!(
        "rm -rf {bak} && {{ [ ! -e {src} ] || mv -f {src} {bak}; }} && \
         {{ mv -f {new} {dst} || {{ [ ! -e {bak} ] || mv -f {bak} {src}; false; }}; }} && \
         rm -rf {bak}"
    )
}

/// One shell word, single-quoted.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The AppleScript run when the bundle's folder needs an administrator,
/// with the paths quoted for the shell and then for AppleScript, so a name
/// with an apostrophe or a double quote survives both.
pub fn admin_swap_script(bundle: &Path, new: &Path, dest: &Path) -> String {
    let shell = admin_swap_shell(bundle, new, dest);
    let escaped = shell.replace('\\', "\\\\").replace('"', "\\\"");
    format!("do shell script \"{escaped}\" with administrator privileges")
}

/// Run [`admin_swap_script`] through /usr/bin/osascript: the system asks
/// for an administrator's name and password, never this app. The new copy
/// goes to [`install_destination`] of `bundle`.
pub fn admin_swap(bundle: &Path, new: &Path, names: &BundleNames) -> Result<(), InstallError> {
    let status = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(admin_swap_script(
            bundle,
            new,
            &install_destination(bundle, names),
        ))
        .status()
        .map_err(|e| {
            InstallError::NeedsAdmin(format!("Could not ask for an administrator: {e}"))
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(InstallError::NeedsAdmin(
            "Failed to move the new app into place".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundle_is_found_from_its_executable() {
        assert_eq!(
            bundle_of_exe(Path::new(
                "/Applications/Method Example.app/Contents/MacOS/method-example"
            )),
            Some(PathBuf::from("/Applications/Method Example.app"))
        );
        assert_eq!(
            bundle_of_exe(Path::new("/Users/x/target/release/method-example")),
            None
        );
    }

    #[test]
    fn the_admin_script_survives_an_apostrophe_and_a_quote() {
        let bundle = Path::new("/Applications/Ann's \"Method Example\".app");
        let s = admin_swap_script(bundle, Path::new("/tmp/new"), bundle);
        let shell = admin_swap_shell(bundle, Path::new("/tmp/new"), bundle);
        assert!(
            shell.contains(r#"'/Applications/Ann'\''s "Method Example".app'"#),
            "{shell}"
        );
        assert_eq!(
            s,
            format!(
                "do shell script \"{}\" with administrator privileges",
                shell.replace('\\', "\\\\").replace('"', "\\\"")
            )
        );
        // The shell form runs for real in tests/hostile_archive.rs.
    }

    #[test]
    fn not_gzip_is_refused_before_anything_moves() {
        let dir = tempfile::tempdir().unwrap();
        let err = unpack_app_archive(b"PK\x03\x04", dir.path()).unwrap_err();
        assert_eq!(err.to_string(), "The update is not a gzip archive.");
    }
}
