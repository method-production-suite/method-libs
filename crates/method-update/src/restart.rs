//! The restart into the new bundle, made as the manifest format's own
//! restart makes it (tauri 2.11.5, `restart_macos_app`): on a Mac it reads
//! the NEW bundle's Info.plist for `CFBundleExecutable`, so the executable's
//! name may change between versions, spawns it with this process's
//! arguments, and the caller exits. It starts [`crate::Installed::bundle`],
//! which after the one-time rename is the new path, not the old one.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use crate::install::{InstallError, bundle_executable};

/// The command that starts the new copy: the bundle's executable on a Mac,
/// the file itself elsewhere (an AppImage, a plain binary).
pub fn relaunch_command(installed: &Path, args: &[OsString]) -> Result<Command, InstallError> {
    let program = if installed.extension().is_some_and(|e| e == "app") {
        bundle_executable(installed)?
    } else {
        installed.to_path_buf()
    };
    let mut cmd = Command::new(program);
    cmd.args(args);
    Ok(cmd)
}

/// Start the new copy with this process's own arguments. The caller exits
/// right after, once its window has closed; a spawn that fails means
/// "installed, reopen it yourself", never "failed": the new bundle is
/// already in place.
pub fn relaunch(installed: &Path) -> Result<(), InstallError> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    relaunch_command(installed, &args)?
        .spawn()
        .map(|_| ())
        .map_err(|e| InstallError::Io(format!("failed to restart app: {e}")))
}
