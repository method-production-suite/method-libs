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
//! Before the running bundle moves, the unpacked one must also pass the
//! platform's code signature check ([`crate::codesign`]): on a Mac, a valid
//! `codesign --verify --deep --strict` by the running app's own team. A
//! refusal is [`InstallError::Signature`], and the installed app is left
//! as it was.
//!
//! [`sweep_at_startup`] cleans up after a swap that was interrupted. The
//! updater never reads or checks the bundle identifier: the signatures are
//! what vouch for an update.

use std::io::Cursor;
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;

use crate::codesign::{CodeSignature, SignatureError};
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
    /// The new bundle's code signature was refused ([`crate::codesign`]).
    Signature(SignatureError),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(s) | Self::Archive(s) | Self::NeedsAdmin(s) => f.write_str(s),
            Self::NothingReady => f.write_str("No update is ready to install."),
            Self::Signature(e) => write!(f, "{e}"),
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

/// How the sweep dates a leftover: the moment it became one.
pub type Stamp = fn(&std::fs::Metadata) -> std::time::SystemTime;

/// When a folder was last changed, the sweep's [`Stamp`]. On unix that is
/// its ctime: a swap makes its backup by RENAMING the running bundle, which
/// keeps the bundle's mtime (often days old) but sets the ctime to now, so
/// mtime would make a backup seconds old look stale. Elsewhere, its mtime.
pub fn changed_at(meta: &std::fs::Metadata) -> std::time::SystemTime {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let secs = u64::try_from(meta.ctime()).unwrap_or(0);
        let nanos = u32::try_from(meta.ctime_nsec()).unwrap_or(0);
        std::time::UNIX_EPOCH + std::time::Duration::new(secs, nanos)
    }
    #[cfg(not(unix))]
    {
        meta.modified().unwrap_or(std::time::UNIX_EPOCH)
    }
}

/// The leftovers of an interrupted swap beside `dir/name`
/// (`.<name>.old-<pid>` and `.<name>.update-<pid>`, from any process but
/// `own_pid`; for the app's current and legacy names, those of both). A
/// swap that died between its two renames leaves no bundle at all, only
/// the hidden backup: then the newest backup that still holds a runnable
/// bundle is moved back under the name it had. Once a bundle is there,
/// leftovers older than ten minutes are removed, except `running`: the
/// bundle this process runs from is never deleted, however old. With no
/// bundle and no usable backup nothing is touched. A leftover's age is
/// counted from [`changed_at`].
pub fn sweep_beside(
    dir: &Path,
    name: &str,
    bundle_names: &BundleNames,
    own_pid: u32,
    running: Option<&Path>,
) -> Swept {
    sweep_beside_stamped(dir, name, bundle_names, own_pid, running, changed_at)
}

/// [`sweep_beside`], dating leftovers with `stamp` instead of
/// [`changed_at`]: for tests, which can set an mtime but never a ctime.
pub fn sweep_beside_stamped(
    dir: &Path,
    name: &str,
    bundle_names: &BundleNames,
    own_pid: u32,
    running: Option<&Path>,
    stamp: Stamp,
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
            (names.contains(&n) && pid != own_pid && meta.is_dir())
                .then(|| (e.path(), n.to_string(), tag == "old", stamp(&meta)))
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
    for (path, _, _, made) in leftovers {
        if running.is_some_and(|r| same_place(r, &path)) {
            continue;
        }
        let age = now.duration_since(made).unwrap_or_default();
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
    sweep_at_startup_stamped(exe, names, changed_at)
}

/// [`sweep_at_startup`], dating leftovers with `stamp`: for tests.
pub fn sweep_at_startup_stamped(exe: &Path, names: &BundleNames, stamp: Stamp) -> Swept {
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
    sweep_beside_stamped(dir, name, names, std::process::id(), Some(bundle), stamp)
}

/// Whether an update could be put where the bundle is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Writability {
    /// This account can write beside the bundle.
    Writable,
    /// Only an administrator can ([`admin_swap`] will ask).
    NeedsAdmin,
    /// Nobody can: the bundle is on a read-only volume. A Mac runs an app
    /// it moved aside for safety (App Translocation) or one opened straight
    /// from its disk image that way. The person has to move the app to
    /// Applications first.
    ReadOnly,
}

/// What a failure to write beside the bundle means.
pub fn classify(e: &std::io::Error) -> Writability {
    // EROFS is 30 on macOS and Linux.
    if e.kind() == std::io::ErrorKind::ReadOnlyFilesystem
        || (cfg!(unix) && e.raw_os_error() == Some(30))
    {
        Writability::ReadOnly
    } else if permission(e) {
        Writability::NeedsAdmin
    } else {
        // Anything else is left for the install itself to report.
        Writability::Writable
    }
}

/// A cheap probe before anything is downloaded: make and remove a hidden
/// folder beside `bundle`, where the swap's staging folder would go.
pub fn install_location(bundle: &Path) -> Writability {
    let probe = sibling(bundle, "probe");
    match std::fs::create_dir(&probe) {
        Ok(()) => {
            let _ = std::fs::remove_dir(&probe);
            Writability::Writable
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_dir(&probe);
            Writability::Writable
        }
        Err(e) => classify(&e),
    }
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
/// osascript). The unpacked bundle must first pass this platform's code
/// signature check ([`crate::codesign::platform`]).
pub fn install_app(
    bytes: &[u8],
    bundle: &Path,
    names: &BundleNames,
    admin: Admin<'_>,
) -> Result<Installed, InstallError> {
    install_app_with(bytes, bundle, names, admin, crate::codesign::platform())
}

/// [`install_app`] with the code signature tool supplied (tests stand in
/// for `/usr/bin/codesign` here). `bundle` is the running app's bundle: its
/// team is the one the new bundle must carry.
pub fn install_app_with(
    bytes: &[u8],
    bundle: &Path,
    names: &BundleNames,
    admin: Admin<'_>,
    signing: &dyn CodeSignature,
) -> Result<Installed, InstallError> {
    let dest = install_destination(bundle, names);
    let staging = sibling(&dest, "update");
    let _ = std::fs::remove_dir_all(&staging);
    match std::fs::create_dir(&staging) {
        Ok(()) => {}
        Err(e) if permission(&e) => {
            return install_with_admin(bytes, bundle, names, admin, signing);
        }
        Err(e) => return Err(io("create", &staging, e)),
    }
    let staged = (|| {
        unpack_app_archive(bytes, &staging)?;
        bundle_executable(&staging)?;
        crate::codesign::check_and_say(bundle, &staging, signing).map_err(InstallError::Signature)
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
            return install_with_admin(bytes, bundle, names, admin, signing);
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
    signing: &dyn CodeSignature,
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
        crate::codesign::check_and_say(bundle, &temp, signing).map_err(InstallError::Signature)?;
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

/// What the administrator's script exits with when root's own copy of the
/// new bundle fails the code signature check.
pub const ADMIN_UNVERIFIED: i32 = 5;

/// The swap itself, as shell: the old app moved aside, `new` moved in at
/// `dest`, the old one put back if that move fails (removing it first would
/// leave no app at all when the move failed after it), then the old one
/// removed.
fn swap_shell(bundle: &Path, new: &str, dest: &Path) -> String {
    let src = shell_quote(&bundle.to_string_lossy());
    let bak = shell_quote(&sibling(bundle, "old").to_string_lossy());
    let dst = shell_quote(&dest.to_string_lossy());
    format!(
        "/bin/rm -rf {bak} && {{ [ ! -e {src} ] || /bin/mv -f {src} {bak}; }} && \
         {{ /bin/mv -f {new} {dst} || {{ [ ! -e {bak} ] || /bin/mv -f {bak} {src}; false; }}; }} && \
         /bin/rm -rf {bak}"
    )
}

/// The shell command an administrator runs when no code signature check
/// applies (the running app has no team): the old app moved aside, the new
/// one moved in at `dest` (the old app's own path, or the current name
/// beside it: [`install_destination`]), and the old one put back if that
/// move fails, then the old one removed.
pub fn admin_swap_shell(bundle: &Path, new: &Path, dest: &Path) -> String {
    swap_shell(bundle, &shell_quote(&new.to_string_lossy()), dest)
}

/// The shell command an administrator runs for an app signed by `team`.
/// `new` was checked already, but it sits in a folder this account owns,
/// so a process of the same account could change it before root moves it.
/// So root first takes its OWN copy, beside `dest`, runs
/// `/usr/bin/codesign --verify --deep --strict` on that copy with the same
/// requirement as the first check ([`crate::codesign::requirement`]), and
/// swaps in that copy. A copy that fails is removed, nothing else moves,
/// and the script exits [`ADMIN_UNVERIFIED`]. `Err` when `team` is not a
/// Team ID.
pub fn admin_swap_shell_checked(
    bundle: &Path,
    new: &Path,
    dest: &Path,
    team: &str,
) -> Result<String, String> {
    let requirement = shell_quote(&format!("-R={}", crate::codesign::requirement(team)?));
    let copy = shell_quote(&sibling(dest, "update").to_string_lossy());
    let new = shell_quote(&new.to_string_lossy());
    let codesign = crate::codesign::CODESIGN;
    let swap = swap_shell(bundle, &copy, dest);
    Ok(format!(
        "/bin/rm -rf {copy} && {DITTO} {new} {copy} && \
         if ! {codesign} --verify --deep --strict {requirement} {copy}; then \
         /bin/rm -rf {copy}; exit {ADMIN_UNVERIFIED}; fi && \
         {{ {swap} || {{ /bin/rm -rf {copy}; false; }}; }}"
    ))
}

/// Copies a bundle as it is, links and all.
pub const DITTO: &str = "/usr/bin/ditto";

/// One shell word, single-quoted.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The AppleScript run when the bundle's folder needs an administrator,
/// with the paths quoted for the shell and then for AppleScript, so a name
/// with an apostrophe or a double quote survives both. With a `team`, the
/// script is [`admin_swap_shell_checked`]; without, [`admin_swap_shell`].
pub fn admin_swap_script(
    bundle: &Path,
    new: &Path,
    dest: &Path,
    team: Option<&str>,
) -> Result<String, String> {
    let shell = match team {
        Some(team) => admin_swap_shell_checked(bundle, new, dest, team)?,
        None => admin_swap_shell(bundle, new, dest),
    };
    let escaped = shell.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!(
        "do shell script \"{escaped}\" with administrator privileges"
    ))
}

/// Run [`admin_swap_script`] through /usr/bin/osascript: the system asks
/// for an administrator's name and password, never this app. The new copy
/// goes to [`install_destination`] of `bundle`. When `bundle` (the running
/// app) is signed by a team, root checks its own copy of `new` against
/// that team before the swap.
pub fn admin_swap(bundle: &Path, new: &Path, names: &BundleNames) -> Result<(), InstallError> {
    admin_swap_with(bundle, new, names, crate::codesign::platform())
}

/// [`admin_swap`] with the code signature tool that reads the running
/// app's team supplied.
pub fn admin_swap_with(
    bundle: &Path,
    new: &Path,
    names: &BundleNames,
    signing: &dyn CodeSignature,
) -> Result<(), InstallError> {
    let team = crate::codesign::expected_team(bundle, signing).map_err(InstallError::Signature)?;
    let script = admin_swap_script(
        bundle,
        new,
        &install_destination(bundle, names),
        team.as_deref(),
    )
    .map_err(|detail| InstallError::Signature(SignatureError::RunningUnreadable { detail }))?;
    let out = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| {
            InstallError::NeedsAdmin(format!("Could not ask for an administrator: {e}"))
        })?;
    if out.status.success() {
        return Ok(());
    }
    // osascript ends its complaint with the script's exit status: "(5)".
    let said = String::from_utf8_lossy(&out.stderr);
    match team {
        Some(team) if said.trim_end().ends_with(&format!("({ADMIN_UNVERIFIED})")) => {
            Err(InstallError::Signature(SignatureError::Invalid {
                team,
                detail: "the copy the administrator step made failed the check".into(),
            }))
        }
        _ => Err(InstallError::NeedsAdmin(
            "Failed to move the new app into place".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_only_volume_is_told_from_a_folder_that_needs_an_administrator() {
        let erofs = std::io::Error::from(std::io::ErrorKind::ReadOnlyFilesystem);
        assert_eq!(classify(&erofs), Writability::ReadOnly);
        #[cfg(unix)]
        assert_eq!(
            classify(&std::io::Error::from_raw_os_error(30)),
            Writability::ReadOnly,
            "EROFS, as a translocated app or a mounted disk image answers"
        );
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(classify(&denied), Writability::NeedsAdmin);
        let other = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(
            classify(&other),
            Writability::Writable,
            "the install says why"
        );
    }

    #[test]
    fn the_probe_writes_beside_the_bundle_and_leaves_nothing() {
        let apps = tempfile::tempdir().unwrap();
        let bundle = apps.path().join("Method Example.app");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        assert_eq!(install_location(&bundle), Writability::Writable);
        let left: Vec<_> = std::fs::read_dir(apps.path()).unwrap().collect();
        assert_eq!(left.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(apps.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
            let found = install_location(&bundle);
            std::fs::set_permissions(apps.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(found, Writability::NeedsAdmin);
        }
    }

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
        let s = admin_swap_script(bundle, Path::new("/tmp/new"), bundle, None).unwrap();
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

    /// Root takes its own copy, checks that copy, and only then swaps it
    /// in; a copy that fails is removed before anything moves.
    #[test]
    fn the_administrator_step_checks_its_own_copy_before_the_swap() {
        let bundle = Path::new("/Applications/Method Example.app");
        let new = Path::new("/private/tmp/method-update-1/Method Example.app");
        let script = admin_swap_shell_checked(bundle, new, bundle, "ABCDE12345").unwrap();
        let at = |needle: &str| {
            script
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} in {script}"))
        };
        let codesign = crate::codesign::CODESIGN;
        assert!(at(DITTO) < at(codesign) && at(codesign) < at("/bin/mv"));
        assert!(
            script.contains(&shell_quote(
                "-R=anchor apple generic and certificate leaf[subject.OU] = \"ABCDE12345\""
            )),
            "the same team requirement, quoted: {script}"
        );
        assert!(script.contains("--verify --deep --strict"));
        assert!(script.contains("exit 5"));
        // What moves in is root's copy, never the folder this account owns.
        let copy = shell_quote(&sibling(bundle, "update").to_string_lossy());
        assert!(script.contains(&format!("/bin/mv -f {copy} ")));
        assert!(!script.contains(&format!(
            "/bin/mv -f {}",
            shell_quote(&new.to_string_lossy())
        )));
        // Nothing but a Team ID reaches root's script.
        assert!(admin_swap_shell_checked(bundle, new, bundle, "X\" or anchor trusted").is_err());
        assert!(admin_swap_script(bundle, new, bundle, Some("';rm -rf /;'")).is_err());
        let apple = admin_swap_script(bundle, new, bundle, Some("ABCDE12345")).unwrap();
        assert!(
            apple.contains("subject.OU] = \\\"ABCDE12345\\\""),
            "{apple}"
        );
    }

    /// The script itself, run as this account in a scratch folder with
    /// stand-ins for ditto and codesign.
    #[cfg(unix)]
    #[test]
    fn the_administrator_script_refuses_a_copy_that_fails_and_installs_one_that_passes() {
        let apps = tempfile::tempdir().unwrap();
        let theirs = tempfile::tempdir().unwrap();
        let bundle = apps.path().join("Method Example.app");
        let new = theirs.path().join("Method Example.app");
        let copy = sibling(&bundle, "update");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("v"), "old").unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("v"), "new").unwrap();
        let run = |codesign: &str| {
            let script = admin_swap_shell_checked(&bundle, &new, &bundle, "ABCDE12345")
                .unwrap()
                .replace(DITTO, "/bin/cp -R")
                .replace(crate::codesign::CODESIGN, codesign);
            std::process::Command::new("/bin/sh")
                .args(["-c", &script])
                .status()
                .unwrap()
                .code()
        };
        assert_eq!(run("false"), Some(ADMIN_UNVERIFIED));
        assert_eq!(std::fs::read_to_string(bundle.join("v")).unwrap(), "old");
        assert!(!copy.exists(), "the failed copy is removed");
        assert!(!sibling(&bundle, "old").exists(), "nothing moved");

        // The copy root checks is the one it took: a swap of the source
        // AFTER the copy (here, by the stand-in for codesign itself, which
        // runs after ditto) never reaches the installed app.
        let swap = format!(
            "/bin/sh -c 'echo swapped > \"$0\"' {}",
            shell_quote(&new.join("v").to_string_lossy())
        );
        assert_eq!(run(&format!("{swap} && true")), Some(0));
        assert_eq!(std::fs::read_to_string(new.join("v")).unwrap(), "swapped\n");
        assert_eq!(
            std::fs::read_to_string(bundle.join("v")).unwrap(),
            "new",
            "the checked copy went in"
        );
        assert!(!copy.exists() && !sibling(&bundle, "old").exists());
    }

    #[test]
    fn not_gzip_is_refused_before_anything_moves() {
        let dir = tempfile::tempdir().unwrap();
        let err = unpack_app_archive(b"PK\x03\x04", dir.path()).unwrap_err();
        assert_eq!(err.to_string(), "The update is not a gzip archive.");
    }
}
