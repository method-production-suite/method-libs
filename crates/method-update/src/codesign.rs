//! The second check on a downloaded bundle, after its minisign signature:
//! on a Mac, the new bundle's code signature must be valid and made by the
//! same team as the app that is running now.
//!
//! The expected team comes from the running bundle itself (`codesign -dv`,
//! its `TeamIdentifier`), so no app has to configure it. Then the staged
//! bundle must pass
//! `codesign --verify --deep --strict` against a requirement that names
//! that team under Apple's anchor (a certificate made by anyone else
//! cannot claim the team), and its own `TeamIdentifier` must be the same.
//! A failure stops the swap before the running bundle moves.
//!
//! A running app that is unsigned, ad-hoc signed or signed without a team
//! (a development build, a test) has no team to compare with: the check is
//! skipped and says so on stderr. So does a running bundle `codesign`
//! cannot read at all. Only the running app decides this, never the
//! download, so a download can not switch the check off.
//!
//! The tool is the [`CodeSignature`] trait, so tests can stand in for
//! `/usr/bin/codesign` without signed bundles. [`platform`] is the real
//! one: `codesign` on a Mac; on Windows and Linux a tool that reports no
//! check (`Signing::Unsupported`), so nothing changes there, and a check
//! for those platforms can be added as another implementation.

use std::path::Path;
use std::process::Command;

/// The full path of the tool, never one looked up on `PATH`.
pub const CODESIGN: &str = "/usr/bin/codesign";

/// Who signed a bundle, as far as the check cares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signing {
    /// Signed by an Apple developer team, with this Team ID.
    Team(String),
    /// Ad-hoc signed (`Signature=adhoc`): no identity at all.
    AdHoc,
    /// Signed, but with no team (`TeamIdentifier=not set`).
    NoTeam,
    /// Not signed at all.
    Unsigned,
    /// This platform has no signature check (Windows, Linux for now).
    Unsupported,
}

/// What the check did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Checked {
    /// The staged bundle is validly signed by this team, the running app's.
    Verified { team: String },
    /// No check was made, and why (the running app has no team).
    Skipped(String),
}

/// Why a downloaded bundle's code signature was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignatureError {
    /// `codesign --verify --deep --strict` failed (or could not run) for
    /// the new bundle, against the running app's team.
    Invalid { team: String, detail: String },
    /// The new bundle's TeamIdentifier is not the running app's.
    TeamMismatch {
        expected: String,
        found: Option<String>,
    },
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid { team, detail } => write!(
                f,
                "The update's code signature is not valid for team {team}, so it was not installed: {detail}"
            ),
            Self::TeamMismatch {
                expected,
                found: Some(found),
            } => write!(
                f,
                "The update is signed by team {found}, not by team {expected} that signed this app, so it was not installed."
            ),
            Self::TeamMismatch {
                expected,
                found: None,
            } => write!(
                f,
                "The update is not signed by team {expected} that signed this app, so it was not installed."
            ),
        }
    }
}

impl std::error::Error for SignatureError {}

/// The two questions the check asks of a platform's signing tool.
pub trait CodeSignature {
    /// Who signed `bundle` (`codesign -dv`). `Err` when the tool could not
    /// read it at all.
    fn signing(&self, bundle: &Path) -> Result<Signing, String>;
    /// Whether `bundle`, all of it, is validly signed by `team`
    /// (`codesign --verify --deep --strict` with a requirement naming it).
    fn verify(&self, bundle: &Path, team: &str) -> Result<(), String>;
}

/// `/usr/bin/codesign`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Codesign;

impl CodeSignature for Codesign {
    fn signing(&self, bundle: &Path) -> Result<Signing, String> {
        let out = Command::new(CODESIGN)
            .arg("-dv")
            .arg(bundle)
            .output()
            .map_err(|e| format!("could not run {CODESIGN}: {e}"))?;
        // codesign -dv writes its answer to stderr.
        let text = String::from_utf8_lossy(&out.stderr);
        parse_display(out.status.success(), &text)
    }

    fn verify(&self, bundle: &Path, team: &str) -> Result<(), String> {
        let out = Command::new(CODESIGN)
            .args(["--verify", "--deep", "--strict"])
            .arg(format!("-R={}", requirement(team)?))
            .arg(bundle)
            .output()
            .map_err(|e| format!("could not run {CODESIGN}: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }
}

/// The tool for a platform with no signature check yet: every bundle is
/// [`Signing::Unsupported`], so the check is always skipped.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCheck;

impl CodeSignature for NoCheck {
    fn signing(&self, _bundle: &Path) -> Result<Signing, String> {
        Ok(Signing::Unsupported)
    }

    fn verify(&self, _bundle: &Path, _team: &str) -> Result<(), String> {
        Err("this platform has no code signature check".into())
    }
}

/// This platform's tool: [`Codesign`] on a Mac, [`NoCheck`] elsewhere.
pub fn platform() -> &'static dyn CodeSignature {
    #[cfg(target_os = "macos")]
    {
        &Codesign
    }
    #[cfg(not(target_os = "macos"))]
    {
        &NoCheck
    }
}

/// A Team ID as Apple issues them: ten letters and digits. Checked before
/// one goes into a requirement, so nothing else can.
fn valid_team(team: &str) -> bool {
    !team.is_empty() && team.len() <= 32 && team.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The requirement the new bundle must satisfy: signed under Apple's
/// anchor by a certificate issued to `team`.
pub fn requirement(team: &str) -> Result<String, String> {
    if !valid_team(team) {
        return Err(format!("{team:?} is not a Team ID"));
    }
    Ok(format!(
        "anchor apple generic and certificate leaf[subject.OU] = \"{team}\""
    ))
}

/// `codesign -dv`'s answer read: `ok` is its exit status, `text` its
/// stderr.
pub fn parse_display(ok: bool, text: &str) -> Result<Signing, String> {
    if !ok {
        return if text.contains("code object is not signed at all") {
            Ok(Signing::Unsigned)
        } else {
            Err(text.trim().to_string())
        };
    }
    let mut team = None;
    let mut adhoc = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "Signature=adhoc" {
            adhoc = true;
        }
        if let Some(t) = line.strip_prefix("TeamIdentifier=") {
            team = Some(t.trim());
        }
        if line.starts_with("CodeDirectory ") && line.contains("(adhoc") {
            adhoc = true;
        }
    }
    if adhoc {
        return Ok(Signing::AdHoc);
    }
    match team {
        None | Some("not set") => Ok(Signing::NoTeam),
        Some(t) if valid_team(t) => Ok(Signing::Team(t.to_string())),
        Some(t) => Err(format!("unreadable TeamIdentifier {t:?}")),
    }
}

/// The check: `staged` (the unpacked update) against `running` (the bundle
/// it replaces, which this app runs from). `Ok` means the swap may go on;
/// [`Checked::Skipped`] says why nothing was compared.
pub fn check_update(
    running: &Path,
    staged: &Path,
    tool: &dyn CodeSignature,
) -> Result<Checked, SignatureError> {
    let team = match tool.signing(running) {
        Ok(Signing::Team(team)) => team,
        Ok(other) => {
            return Ok(Checked::Skipped(match other {
                Signing::AdHoc => "the running app is ad-hoc signed".into(),
                Signing::NoTeam => "the running app is signed without a team".into(),
                Signing::Unsigned => "the running app is not signed".into(),
                _ => "this platform has no code signature check".into(),
            }));
        }
        Err(e) => {
            return Ok(Checked::Skipped(format!(
                "the running app's signature could not be read ({e})"
            )));
        }
    };
    tool.verify(staged, &team)
        .map_err(|detail| SignatureError::Invalid {
            team: team.clone(),
            detail,
        })?;
    match tool.signing(staged) {
        Ok(Signing::Team(found)) if found == team => Ok(Checked::Verified { team }),
        Ok(Signing::Team(found)) => Err(SignatureError::TeamMismatch {
            expected: team,
            found: Some(found),
        }),
        _ => Err(SignatureError::TeamMismatch {
            expected: team,
            found: None,
        }),
    }
}

/// [`check_update`], with a skipped check said on stderr (the crate has no
/// logger; an app that wants its own line can call [`check_update`]).
pub(crate) fn check_and_say(
    running: &Path,
    staged: &Path,
    tool: &dyn CodeSignature,
) -> Result<(), SignatureError> {
    if let Checked::Skipped(why) = check_update(running, staged, tool)? {
        eprintln!(
            "method-update: code signature check skipped for {}: {why}",
            running.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Stands in for codesign: a signing per bundle, and whether each
    /// verifies.
    #[derive(Default)]
    struct Fake {
        signing: HashMap<PathBuf, Result<Signing, String>>,
        valid: HashMap<PathBuf, bool>,
        asked: Mutex<Vec<String>>,
    }

    impl CodeSignature for Fake {
        fn signing(&self, bundle: &Path) -> Result<Signing, String> {
            self.asked
                .lock()
                .unwrap()
                .push(format!("dv {}", bundle.display()));
            self.signing
                .get(bundle)
                .cloned()
                .unwrap_or_else(|| Err("not found".into()))
        }
        fn verify(&self, bundle: &Path, team: &str) -> Result<(), String> {
            self.asked
                .lock()
                .unwrap()
                .push(format!("verify {} {team}", bundle.display()));
            if self.valid.get(bundle).copied().unwrap_or(false) {
                Ok(())
            } else {
                Err("a sealed resource is missing or invalid".into())
            }
        }
    }

    fn paths() -> (PathBuf, PathBuf) {
        (
            PathBuf::from("/Applications/Method Example.app"),
            PathBuf::from("/Applications/.Method Example.app.update-1"),
        )
    }

    fn fake(running: Result<Signing, String>, staged: Signing, valid: bool) -> Fake {
        let (r, s) = paths();
        let mut f = Fake::default();
        f.signing.insert(r, running);
        f.signing.insert(s.clone(), Ok(staged));
        f.valid.insert(s, valid);
        f
    }

    #[test]
    fn the_same_team_and_a_valid_signature_pass() {
        let (r, s) = paths();
        let f = fake(
            Ok(Signing::Team("ABCDE12345".into())),
            Signing::Team("ABCDE12345".into()),
            true,
        );
        assert_eq!(
            check_update(&r, &s, &f).unwrap(),
            Checked::Verified {
                team: "ABCDE12345".into()
            }
        );
    }

    #[test]
    fn a_mismatched_team_is_refused() {
        let (r, s) = paths();
        let f = fake(
            Ok(Signing::Team("ABCDE12345".into())),
            Signing::Team("ZZZZZ99999".into()),
            true,
        );
        assert_eq!(
            check_update(&r, &s, &f).unwrap_err(),
            SignatureError::TeamMismatch {
                expected: "ABCDE12345".into(),
                found: Some("ZZZZZ99999".into())
            }
        );
        // An ad-hoc or unsigned update never stands in for the team.
        for staged in [Signing::AdHoc, Signing::Unsigned, Signing::NoTeam] {
            let f = fake(Ok(Signing::Team("ABCDE12345".into())), staged, true);
            assert!(matches!(
                check_update(&r, &s, &f).unwrap_err(),
                SignatureError::TeamMismatch { found: None, .. }
            ));
        }
    }

    #[test]
    fn a_signature_that_does_not_verify_is_refused() {
        let (r, s) = paths();
        let f = fake(
            Ok(Signing::Team("ABCDE12345".into())),
            Signing::Team("ABCDE12345".into()),
            false,
        );
        let err = check_update(&r, &s, &f).unwrap_err();
        assert!(matches!(&err, SignatureError::Invalid { team, .. } if team == "ABCDE12345"));
        assert!(err.to_string().contains("not valid for team ABCDE12345"));
        assert!(
            f.asked
                .lock()
                .unwrap()
                .contains(&format!("verify {} ABCDE12345", s.display())),
            "verified against the running app's team"
        );
    }

    #[test]
    fn an_unsigned_or_ad_hoc_running_app_skips_the_check() {
        let (r, s) = paths();
        for running in [
            Ok(Signing::Unsigned),
            Ok(Signing::AdHoc),
            Ok(Signing::NoTeam),
            Ok(Signing::Unsupported),
            Err("bundle format unrecognized".into()),
        ] {
            // Even an update that would fail is not looked at.
            let f = fake(running.clone(), Signing::Unsigned, false);
            assert!(
                matches!(check_update(&r, &s, &f), Ok(Checked::Skipped(_))),
                "{running:?}"
            );
            assert_eq!(f.asked.lock().unwrap().len(), 1, "only the running app");
        }
        let f = fake(Ok(Signing::Unsigned), Signing::Unsigned, false);
        assert_eq!(
            check_update(&r, &s, &f).unwrap(),
            Checked::Skipped("the running app is not signed".into())
        );
        assert!(check_and_say(&r, &s, &NoCheck).is_ok());
    }

    #[test]
    fn codesign_s_answers_are_read() {
        let developer_id = "Executable=/Applications/X.app/Contents/MacOS/X\n\
            Identifier=com.example.x\n\
            Format=app bundle with Mach-O universal (x86_64 arm64)\n\
            CodeDirectory v=20500 size=542 flags=0x10000(runtime) hashes=6+7 location=embedded\n\
            Signature size=8978\n\
            TeamIdentifier=ABCDE12345\n";
        assert_eq!(
            parse_display(true, developer_id),
            Ok(Signing::Team("ABCDE12345".into()))
        );
        let adhoc = "CodeDirectory v=20100 size=223 flags=0x2(adhoc) hashes=1+3 location=embedded\n\
            Signature=adhoc\n\
            TeamIdentifier=not set\n";
        assert_eq!(parse_display(true, adhoc), Ok(Signing::AdHoc));
        assert_eq!(
            parse_display(true, "Signature size=4000\nTeamIdentifier=not set\n"),
            Ok(Signing::NoTeam)
        );
        assert_eq!(
            parse_display(false, "X.app: code object is not signed at all\n"),
            Ok(Signing::Unsigned)
        );
        assert!(parse_display(false, "X.app: No such file or directory").is_err());
        assert!(parse_display(true, "TeamIdentifier=AB\"or\n").is_err());
    }

    #[test]
    fn only_a_team_id_goes_into_a_requirement() {
        assert_eq!(
            requirement("ABCDE12345").unwrap(),
            "anchor apple generic and certificate leaf[subject.OU] = \"ABCDE12345\""
        );
        assert!(requirement("X\" or anchor trusted").is_err());
        assert!(requirement("").is_err());
    }

    #[test]
    fn other_platforms_check_nothing() {
        assert_eq!(NoCheck.signing(Path::new("/x")), Ok(Signing::Unsupported));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            platform().signing(Path::new("/x")),
            Ok(Signing::Unsupported)
        );
    }

    /// The real tool on a Mac, against a bundle it can't call signed.
    #[cfg(target_os = "macos")]
    #[test]
    fn real_codesign_skips_an_unsigned_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Unsigned.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/MacOS/x"), b"#!/bin/sh\n").unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleExecutable</key><string>x</string></dict></plist>",
        )
        .unwrap();
        assert_eq!(Codesign.signing(&app), Ok(Signing::Unsigned));
        assert!(matches!(
            check_update(&app, &app, &Codesign),
            Ok(Checked::Skipped(_))
        ));
    }
}
