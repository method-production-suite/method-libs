//! The suite's update behaviour (SUITE-STANDARDS.md, section 1), without
//! any UI framework, so an egui window and a Tauri window can share it:
//!
//! - the schedule: a first check 2 minutes after launch, then one every
//!   6 hours while the app runs ([`Schedule`]);
//! - the "Check for Updates Automatically" switch, on by default, kept in
//!   a small JSON file the app names, written atomically ([`Settings`]);
//! - whether What's New is due after an update ([`whats_new_due`]);
//! - the state machine a window draws: idle, checking, downloading, ready,
//!   installing, failed ([`Machine`]). An automatic check that fails says
//!   nothing; a check the person asked for always answers;
//! - [`Updater`], which ties these to the network and the bundle swap.
//!
//! Nothing here starts a thread or a timer. The app's own timer (or a
//! thread of its own) asks [`Updater::due`] now and then and runs
//! [`Updater::check_now`] off the UI thread when it says so; the UI reads
//! [`Updater::status`].

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::config::AppConfig;
use crate::install::{Admin, InstallError, Installed, install_app};
use crate::target::Installer;
use crate::transport::{Update, check, download};

/// The first automatic check, after the app starts.
pub const FIRST_CHECK: Duration = Duration::from_secs(2 * 60);
/// Then one every this long, while the app runs.
pub const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// How often an app's timer might ask [`Updater::due`]; any period well
/// under [`FIRST_CHECK`] will do.
pub const TICK: Duration = Duration::from_secs(30);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// When the next automatic check is due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schedule {
    next: Instant,
    every: Duration,
}

impl Schedule {
    /// The suite's schedule for an app launched at `launched`.
    pub fn starting(launched: Instant) -> Self {
        Self::new(launched, FIRST_CHECK, CHECK_EVERY)
    }

    /// Any other schedule (tests, a rehearsal).
    pub fn new(launched: Instant, first: Duration, every: Duration) -> Self {
        Self {
            next: launched + first,
            every,
        }
    }

    /// True once a check is due at `now`; the next one is then `every`
    /// from now (a Mac that slept through several periods checks once).
    pub fn due(&mut self, now: Instant) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now + self.every;
        true
    }

    pub fn next(&self) -> Instant {
        self.next
    }
}

/// The updater's settings, in the JSON file the app names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Check now and then without being asked. On unless turned off.
    pub automatic: bool,
    /// The version whose What's New was shown last (or that ran first).
    pub seen: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            automatic: true,
            seen: None,
        }
    }
}

impl Settings {
    /// Reads `file`; missing or unreadable means the defaults.
    pub fn load(file: &Path) -> Settings {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Writes `file` atomically, making its folder if needed.
    pub fn save(&self, file: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        crate::fsutil::write_atomically(file, &bytes)
    }
}

/// Whether What's New is due for `current`, given the version last `seen`:
/// (show it, the version to remember now). A first run shows nothing (it
/// isn't an update); a newer version shows it (and is remembered once
/// shown); going back to an older one shows nothing.
pub fn whats_new_due(seen: Option<&str>, current: &str) -> (bool, Option<String>) {
    let Ok(now) = crate::manifest::parse_version(current) else {
        return (false, None);
    };
    match seen.map(crate::manifest::parse_version) {
        None | Some(Err(_)) => (false, Some(current.to_string())),
        Some(Ok(before)) if before < now => (true, None),
        Some(Ok(before)) if before == now => (false, None),
        Some(Ok(_)) => (false, Some(current.to_string())),
    }
}

/// Where an update is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// `manual`: the person asked (Help > Check for Updates), so the window
    /// says what is happening and how it ended.
    Checking {
        manual: bool,
    },
    Downloading {
        version: String,
        manual: bool,
    },
    /// Downloaded and its signature checked: "Restart to update" installs it.
    Ready {
        version: String,
    },
    Installing {
        version: String,
    },
    /// A check (or its download) the person asked for didn't work. An
    /// automatic one never comes here: it goes back to idle, silently.
    Failed {
        message: String,
    },
}

/// A one-time answer the window shows in its quiet place, then lets go.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Said {
    /// A check the person asked for found nothing newer.
    Current,
    /// A check (or its download) the person asked for didn't work.
    Failed,
    /// Restart to update couldn't install; the update is still ready.
    InstallFailed,
}

/// What a check asks for next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// Go ahead and check.
    Check,
    /// A check or download is already under way (now counted as asked for,
    /// if this one was).
    Busy,
    /// An update is ready or installing already.
    Ready,
}

/// The state machine of an update. Every change goes through here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    pub phase: Phase,
}

impl Default for Machine {
    fn default() -> Self {
        Machine { phase: Phase::Idle }
    }
}

impl Machine {
    pub fn start_check(&mut self, manual: bool) -> Start {
        match &mut self.phase {
            Phase::Idle | Phase::Failed { .. } => {
                self.phase = Phase::Checking { manual };
                Start::Check
            }
            Phase::Checking { manual: m } | Phase::Downloading { manual: m, .. } => {
                *m |= manual;
                Start::Busy
            }
            Phase::Ready { .. } | Phase::Installing { .. } => Start::Ready,
        }
    }

    /// The check finished: `Ok(Some(v))` found version `v`, `Ok(None)` found
    /// nothing newer, `Err` didn't work. True when `v` should be
    /// downloaded; and what to tell a person who asked.
    pub fn checked(&mut self, found: Result<Option<String>, String>) -> (bool, Option<Said>) {
        let Phase::Checking { manual } = self.phase else {
            return (false, None);
        };
        match found {
            Ok(Some(version)) => {
                self.phase = Phase::Downloading { version, manual };
                (true, None)
            }
            Ok(None) => {
                self.phase = Phase::Idle;
                (false, manual.then_some(Said::Current))
            }
            Err(message) => (false, self.fail(manual, message)),
        }
    }

    /// The download finished: `Ok` downloaded and its signature checked.
    pub fn downloaded(&mut self, result: Result<(), String>) -> Option<Said> {
        let Phase::Downloading { version, manual } = &self.phase else {
            return None;
        };
        let manual = *manual;
        match result {
            Ok(()) => {
                self.phase = Phase::Ready {
                    version: version.clone(),
                };
                None
            }
            Err(message) => self.fail(manual, message),
        }
    }

    /// A failure: reported when the person asked, silent otherwise.
    fn fail(&mut self, manual: bool, message: String) -> Option<Said> {
        if manual {
            self.phase = Phase::Failed { message };
            Some(Said::Failed)
        } else {
            self.phase = Phase::Idle;
            None
        }
    }

    /// The person has seen a failure: back to idle.
    pub fn dismiss(&mut self) {
        if matches!(self.phase, Phase::Failed { .. }) {
            self.phase = Phase::Idle;
        }
    }

    /// Restart to update was chosen: the version to install, if one is
    /// ready.
    pub fn start_install(&mut self) -> Option<String> {
        let Phase::Ready { version } = &self.phase else {
            return None;
        };
        let v = version.clone();
        self.phase = Phase::Installing { version: v.clone() };
        Some(v)
    }

    /// Installing failed: the update stays ready for another try.
    pub fn install_failed(&mut self) -> Said {
        if let Phase::Installing { version } = &self.phase {
            self.phase = Phase::Ready {
                version: version.clone(),
            };
        }
        Said::InstallFailed
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.phase, Phase::Ready { .. })
    }

    /// What the window is told.
    pub fn status(&self, automatic: bool, available: bool, said: Option<Said>) -> Status {
        let (phase, version, manual, message) = match &self.phase {
            Phase::Idle => ("idle", None, false, None),
            Phase::Checking { manual } => ("checking", None, *manual, None),
            Phase::Downloading { version, manual } => {
                ("downloading", Some(version.clone()), *manual, None)
            }
            Phase::Ready { version } => ("ready", Some(version.clone()), false, None),
            Phase::Installing { version } => ("installing", Some(version.clone()), false, None),
            Phase::Failed { message } => ("failed", None, true, Some(message.clone())),
        };
        Status {
            phase,
            version,
            manual,
            message,
            said,
            automatic,
            available,
        }
    }
}

/// What the window is told; serializable, so a Tauri app can emit it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    /// "idle", "checking", "downloading", "ready", "installing" or "failed".
    pub phase: &'static str,
    /// The update's version, once one was found.
    pub version: Option<String>,
    /// The person asked for this check: show its progress.
    pub manual: bool,
    /// Why a check the person asked for failed.
    pub message: Option<String>,
    /// A one-time answer to show (then let go).
    pub said: Option<Said>,
    /// Automatic checks are on.
    pub automatic: bool,
    /// This build can update itself at all.
    pub available: bool,
}

/// The update policy for one running app: its settings, its schedule, its
/// state machine and the update it downloaded. Shareable across threads
/// (put it in an `Arc`); every method takes `&self`.
pub struct Updater {
    app: AppConfig,
    current: String,
    version: Option<Version>,
    unavailable: Option<String>,
    settings_file: Option<PathBuf>,
    settings: Mutex<Settings>,
    machine: Mutex<Machine>,
    pending: Mutex<Option<(Update, Vec<u8>)>>,
    schedule: Mutex<Schedule>,
    whats_new: Mutex<bool>,
}

impl Updater {
    /// The updater for `app`, running version `current`, launched at
    /// `launched`, keeping its settings in `settings_file` (`None`: kept in
    /// memory only). Reads the settings, and decides once whether What's New
    /// is due. A build that cannot update itself
    /// ([`AppConfig::unavailable_reason`], or a version that is not semver)
    /// never checks: no timer, no network.
    pub fn new(
        app: AppConfig,
        current: &str,
        settings_file: Option<PathBuf>,
        launched: Instant,
    ) -> Self {
        let mut settings = settings_file
            .as_deref()
            .map(Settings::load)
            .unwrap_or_default();
        let (due, remember) = whats_new_due(settings.seen.as_deref(), current);
        if let Some(v) = remember {
            settings.seen = Some(v);
            if let Some(f) = &settings_file {
                let _ = settings.save(f);
            }
        }
        let version = crate::manifest::parse_version(current).ok();
        let unavailable = app.unavailable_reason().or_else(|| {
            version
                .is_none()
                .then(|| format!("the running version {current:?} is not semver"))
        });
        Self {
            app,
            current: current.to_string(),
            version,
            unavailable,
            settings_file,
            settings: Mutex::new(settings),
            machine: Mutex::new(Machine::default()),
            pending: Mutex::new(None),
            schedule: Mutex::new(Schedule::starting(launched)),
            whats_new: Mutex::new(due),
        }
    }

    /// The same, on another schedule (tests, a rehearsal).
    pub fn with_schedule(self, schedule: Schedule) -> Self {
        *lock(&self.schedule) = schedule;
        self
    }

    pub fn app(&self) -> &AppConfig {
        &self.app
    }

    /// Why this build can't update itself, or `None` when it can.
    pub fn unavailable(&self) -> Option<&str> {
        self.unavailable.as_deref()
    }

    /// Whether automatic checks are on.
    pub fn automatic(&self) -> bool {
        lock(&self.settings).automatic
    }

    /// Turns automatic checks on or off, and keeps the choice.
    pub fn set_automatic(&self, on: bool) -> std::io::Result<()> {
        let mut s = lock(&self.settings);
        s.automatic = on;
        match &self.settings_file {
            Some(f) => s.save(f),
            None => Ok(()),
        }
    }

    pub fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    pub fn phase(&self) -> Phase {
        lock(&self.machine).phase.clone()
    }

    /// What the window is told, with a one-time answer if there is one.
    pub fn status(&self, said: Option<Said>) -> Status {
        lock(&self.machine).status(self.automatic(), self.unavailable.is_none(), said)
    }

    /// The app's timer asks this now and then: true when an automatic check
    /// should run now (then run [`Self::check_now`] off the UI thread).
    pub fn due(&self, now: Instant) -> bool {
        let due = lock(&self.schedule).due(now);
        due && self.unavailable.is_none() && self.automatic()
    }

    /// One check, then the download if it found something. Blocks: run it
    /// on a thread of its own. `manual`: the person asked. Returns the
    /// one-time answer to show, if any.
    pub fn check_now(&self, manual: bool) -> Option<Said> {
        let config = self.app.transport(Some(Installer::App));
        let current = self.version.clone();
        self.check_with(
            manual,
            || match &current {
                Some(v) => check(&config, v).map_err(|e| e.to_string()),
                None => Err("the running version is not semver".into()),
            },
            |update| download(&config, update, |_, _| {}, || {}).map_err(|e| e.to_string()),
        )
    }

    /// [`Self::check_now`] with the network supplied: `check` asks for a
    /// newer release, `download` fetches and verifies it.
    pub fn check_with(
        &self,
        manual: bool,
        check: impl FnOnce() -> Result<Option<Update>, String>,
        download: impl FnOnce(&Update) -> Result<Vec<u8>, String>,
    ) -> Option<Said> {
        if let Some(reason) = &self.unavailable {
            return manual
                .then(|| {
                    let mut m = lock(&self.machine);
                    m.phase = Phase::Checking { manual: true };
                    m.checked(Err(format!("This copy can't update itself: {reason}.")))
                        .1
                })
                .flatten();
        }
        if lock(&self.machine).start_check(manual) != Start::Check {
            return None;
        }
        let found = check();
        let version = match &found {
            Ok(u) => Ok(u.as_ref().map(|u| u.version.clone())),
            Err(e) => Err(e.clone()),
        };
        let (fetch, said) = lock(&self.machine).checked(version);
        let (true, Ok(Some(update))) = (fetch, found) else {
            return said;
        };
        match download(&update) {
            Ok(bytes) => {
                *lock(&self.pending) = Some((update, bytes));
                lock(&self.machine).downloaded(Ok(()))
            }
            Err(e) => lock(&self.machine).downloaded(Err(e)),
        }
    }

    /// The person has seen a failure: back to idle.
    pub fn dismiss(&self) {
        lock(&self.machine).dismiss();
    }

    /// The update downloaded and verified, waiting for a restart.
    pub fn ready(&self) -> Option<Update> {
        lock(&self.pending).as_ref().map(|(u, _)| u.clone())
    }

    /// "Restart to update": swap `bundle` for the ready update
    /// ([`install_app`], with this app's bundle names). On success the app
    /// restarts into [`Installed::bundle`] ([`crate::restart::relaunch`])
    /// and exits. On failure the update stays ready for another try.
    pub fn install_ready(
        &self,
        bundle: &Path,
        admin: Admin<'_>,
    ) -> Result<Installed, InstallError> {
        if lock(&self.machine).start_install().is_none() {
            return Err(InstallError::NothingReady);
        }
        let pending = lock(&self.pending).take();
        let result = match &pending {
            Some((_, bytes)) => install_app(bytes, bundle, &self.app.bundle, admin),
            None => Err(InstallError::NothingReady),
        };
        if result.is_err() {
            *lock(&self.pending) = pending;
            lock(&self.machine).install_failed();
        }
        result
    }

    /// True once, on the first launch of a newer version: show What's New
    /// now. Remembers that it was shown.
    pub fn take_whats_new(&self) -> bool {
        let due = std::mem::take(&mut *lock(&self.whats_new));
        if due {
            let mut s = lock(&self.settings);
            s.seen = Some(self.current.clone());
            if let Some(f) = &self.settings_file {
                let _ = s.save(f);
            }
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn key() -> String {
        let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        base64::engine::general_purpose::STANDARD.encode(kp.pk.to_box().unwrap().to_string())
    }

    fn app(pubkey: &str) -> AppConfig {
        let pubkey: &'static str = Box::leak(pubkey.to_string().into_boxed_str());
        AppConfig::new(
            "Method Example",
            "Method Example.app",
            "method-example-update/1.0.0",
            crate::Baked::release(
                "https://github.com/example/app/releases/latest/download/latest.json",
                pubkey,
            ),
        )
    }

    fn update(v: &str) -> Update {
        Update {
            current_version: "1.0.0".into(),
            version: v.into(),
            body: None,
            date: None,
            target: "darwin-aarch64".into(),
            download_url: "https://github.com/x".into(),
            signature: "s".into(),
            raw_json: serde_json::Value::Null,
        }
    }

    #[test]
    fn the_schedule_is_two_minutes_then_every_six_hours() {
        let t0 = Instant::now();
        let mut s = Schedule::starting(t0);
        assert!(!s.due(t0));
        assert!(!s.due(t0 + Duration::from_secs(119)));
        assert!(s.due(t0 + FIRST_CHECK));
        assert!(!s.due(t0 + FIRST_CHECK + Duration::from_secs(60)));
        assert!(s.due(t0 + FIRST_CHECK + CHECK_EVERY));
        // A Mac asleep for a day checks once on waking, then waits again.
        let woke = t0 + Duration::from_secs(30 * 60 * 60);
        assert!(s.due(woke));
        assert!(!s.due(woke + Duration::from_secs(1)));
        assert_eq!(s.next(), woke + CHECK_EVERY);
    }

    #[test]
    fn automatic_checks_are_quiet() {
        let mut m = Machine::default();
        assert_eq!(m.start_check(false), Start::Check);
        assert_eq!(m.checked(Err("offline".into())), (false, None));
        assert_eq!(m.phase, Phase::Idle, "no failure shown");
        assert_eq!(m.start_check(false), Start::Check);
        assert_eq!(m.checked(Ok(None)), (false, None));
        assert_eq!(m.start_check(false), Start::Check);
        assert_eq!(m.checked(Ok(Some("1.1.0".into()))), (true, None));
        assert_eq!(m.downloaded(Err("bad signature".into())), None);
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn a_check_the_person_asked_for_answers() {
        let mut m = Machine::default();
        assert_eq!(m.start_check(true), Start::Check);
        assert_eq!(m.checked(Ok(None)), (false, Some(Said::Current)));
        assert_eq!(m.start_check(true), Start::Check);
        assert_eq!(
            m.checked(Err("offline".into())),
            (false, Some(Said::Failed))
        );
        assert_eq!(
            m.phase,
            Phase::Failed {
                message: "offline".into()
            }
        );
        let st = m.status(true, true, None);
        assert_eq!(
            (st.phase, st.message.as_deref()),
            ("failed", Some("offline"))
        );
        // A failure doesn't block the next check; dismissing clears it.
        assert_eq!(m.start_check(false), Start::Check);
        m.phase = Phase::Failed {
            message: "x".into(),
        };
        m.dismiss();
        assert_eq!(m.phase, Phase::Idle);
        // An automatic check under way becomes asked-for when the person
        // asks during it, so its end is reported.
        assert_eq!(m.start_check(false), Start::Check);
        assert_eq!(m.start_check(true), Start::Busy);
        assert_eq!(m.checked(Ok(Some("1.1.0".into()))), (true, None));
        assert_eq!(m.downloaded(Err("x".into())), Some(Said::Failed));
    }

    #[test]
    fn ready_then_installing_and_a_failed_install_stays_ready() {
        let mut m = Machine::default();
        m.start_check(false);
        m.checked(Ok(Some("1.1.0".into())));
        assert_eq!(m.downloaded(Ok(())), None);
        assert!(m.is_ready());
        assert_eq!(m.start_check(true), Start::Ready);
        assert_eq!(m.status(true, true, None).phase, "ready");
        assert_eq!(m.start_install().as_deref(), Some("1.1.0"));
        assert_eq!(m.status(true, true, None).phase, "installing");
        assert_eq!(m.install_failed(), Said::InstallFailed);
        assert!(m.is_ready());
    }

    #[test]
    fn whats_new_once_after_an_update() {
        assert_eq!(whats_new_due(None, "1.0.0"), (false, Some("1.0.0".into())));
        assert_eq!(whats_new_due(Some("0.9.0"), "1.0.0"), (true, None));
        assert_eq!(whats_new_due(Some("1.0.0"), "1.0.0"), (false, None));
        assert_eq!(
            whats_new_due(Some("1.1.0"), "1.0.0"),
            (false, Some("1.0.0".into()))
        );
        assert_eq!(whats_new_due(Some("v0.9.0"), "1.0.0"), (true, None));
        assert_eq!(whats_new_due(Some("0.9.0"), "dev"), (false, None));
    }

    #[test]
    fn settings_default_on_and_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("updates.json");
        assert_eq!(Settings::load(&file), Settings::default());
        assert!(Settings::default().automatic, "on by default");
        std::fs::write(&file, "not json").unwrap();
        assert!(Settings::load(&file).automatic);
        std::fs::write(&file, r#"{"seen":"1.0.0"}"#).unwrap();
        assert!(Settings::load(&file).automatic, "a missing switch is on");
        let off = Settings {
            automatic: false,
            seen: Some("1.0.0".into()),
        };
        off.save(&file).unwrap();
        assert_eq!(Settings::load(&file), off);
    }

    #[test]
    fn the_updater_keeps_the_switch_and_what_s_new() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub/updates.json");
        let k = key();
        let t0 = Instant::now();
        let u = Updater::new(app(&k), "1.0.0", Some(file.clone()), t0);
        assert!(u.unavailable().is_none(), "{:?}", u.unavailable());
        assert!(u.automatic());
        assert!(!u.take_whats_new(), "a first run is not an update");
        assert_eq!(Settings::load(&file).seen.as_deref(), Some("1.0.0"));
        u.set_automatic(false).unwrap();
        assert!(!u.due(t0 + FIRST_CHECK), "off: never due");

        let u = Updater::new(app(&k), "1.1.0", Some(file.clone()), t0);
        assert!(!u.automatic(), "the switch is remembered");
        assert!(u.take_whats_new());
        assert!(!u.take_whats_new(), "once");
        assert_eq!(Settings::load(&file).seen.as_deref(), Some("1.1.0"));
        u.set_automatic(true).unwrap();
        assert!(!u.due(t0));
        assert!(u.due(t0 + FIRST_CHECK));
        assert!(!u.due(t0 + FIRST_CHECK + TICK));
    }

    #[test]
    fn the_updater_runs_a_check_and_keeps_the_download() {
        let u = Updater::new(app(&key()), "1.0.0", None, Instant::now());
        // Automatic and failing: silent.
        assert_eq!(
            u.check_with(false, || Err("offline".into()), |_| unreachable!()),
            None
        );
        assert_eq!(u.phase(), Phase::Idle);
        // Asked for and failing: reported.
        assert_eq!(
            u.check_with(true, || Err("offline".into()), |_| unreachable!()),
            Some(Said::Failed)
        );
        assert_eq!(u.status(None).phase, "failed");
        u.dismiss();
        // Asked for and current.
        assert_eq!(
            u.check_with(true, || Ok(None), |_| unreachable!()),
            Some(Said::Current)
        );
        // Found, but the download didn't verify: silent when automatic.
        assert_eq!(
            u.check_with(false, || Ok(Some(update("1.1.0"))), |_| Err("bad".into())),
            None
        );
        assert!(u.ready().is_none());
        // Found and downloaded.
        assert_eq!(
            u.check_with(false, || Ok(Some(update("1.1.0"))), |_| Ok(vec![1, 2])),
            None
        );
        assert_eq!(u.ready().map(|r| r.version).as_deref(), Some("1.1.0"));
        let st = u.status(None);
        assert_eq!((st.phase, st.version.as_deref()), ("ready", Some("1.1.0")));
        // Nothing more is checked while an update waits.
        assert_eq!(
            u.check_with(true, || unreachable!(), |_| unreachable!()),
            None
        );
    }

    #[test]
    fn a_failed_install_keeps_the_update_ready() {
        let apps = tempfile::tempdir().unwrap();
        let bundle = apps.path().join("Method Example.app");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        let u = Updater::new(app(&key()), "1.0.0", None, Instant::now());
        let no_admin = |_: &Path, _: &Path| -> Result<(), InstallError> { unreachable!() };
        assert!(matches!(
            u.install_ready(&bundle, &no_admin),
            Err(InstallError::NothingReady)
        ));
        u.check_with(
            false,
            || Ok(Some(update("1.1.0"))),
            |_| Ok(b"not gzip".to_vec()),
        );
        let err = u.install_ready(&bundle, &no_admin).unwrap_err();
        assert_eq!(err.to_string(), "The update is not a gzip archive.");
        assert_eq!(u.status(None).phase, "ready");
        assert!(u.ready().is_some(), "still there for another try");
        assert!(bundle.join("Contents").is_dir(), "the old bundle untouched");
    }

    #[test]
    fn a_build_that_cannot_update_never_checks() {
        let u = Updater::new(app("placeholder"), "1.0.0", None, Instant::now());
        assert!(u.unavailable().unwrap().starts_with("no updater key"));
        assert!(!u.due(Instant::now() + FIRST_CHECK));
        assert_eq!(
            u.check_with(false, || unreachable!(), |_| unreachable!()),
            None
        );
        assert_eq!(
            u.check_with(true, || unreachable!(), |_| unreachable!()),
            Some(Said::Failed)
        );
        let st = u.status(None);
        assert!(!st.available);
        assert!(
            st.message
                .unwrap()
                .starts_with("This copy can't update itself")
        );

        let dev = Updater::new(app(&key()), "dev", None, Instant::now());
        assert!(dev.unavailable().unwrap().contains("not semver"));
    }
}
