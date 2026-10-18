//! Download, install, restart, in one order with fixed outcomes, every
//! side effect injected through [`Steps`] so the order is a test rather
//! than a comment.
//!
//! This is the detailed run for an app whose window shows each moment (the
//! download's progress, "Installing", waiting for work to finish, the
//! restart): it maps [`Event`] onto what it draws. An app that only needs
//! "Update ready, restart to update" can use [`crate::policy::Updater`]
//! instead, which keeps the downloaded update until the person restarts.

use crate::transport::Update;

/// One moment of a run.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Downloading {
        version: String,
        received: u64,
        total: Option<u64>,
        body: String,
    },
    Installing {
        version: String,
    },
    /// The bundle is swapped and counted engine work is still running.
    Waiting {
        version: String,
    },
    /// The wait for quiet ran out: the label offers "Restart now".
    Stalled {
        version: String,
    },
    Restarting {
        version: String,
    },
    /// Swapped, but this copy cannot restart itself (or the restart was
    /// refused): "Update installed. Quit and reopen the app to use it".
    Installed {
        version: String,
    },
    Failed {
        version: Option<String>,
        message: String,
    },
    /// A fresh check found nothing newer after all: the label goes away.
    Cleared,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Restarting {
        version: String,
        update: Update,
    },
    Stalled {
        version: String,
        update: Update,
    },
    Installed {
        version: String,
        update: Update,
    },
    Current,
    Error {
        message: String,
        update: Option<Update>,
    },
}

/// Everything the run does to the world.
pub trait Steps {
    /// A fresh check, for a run started from a remembered offer (the click
    /// is the consent for one request, as the manual button is).
    fn fresh_check(&mut self) -> Result<Option<Update>, String>;
    /// Download and verify (`transport::download`), reporting chunks and
    /// the end of the download.
    fn download(
        &mut self,
        update: &Update,
        progress: &mut dyn FnMut(usize, Option<u64>),
        finished: &mut dyn FnMut(),
    ) -> Result<Vec<u8>, String>;
    /// Put the verified bytes in place (`install::install_app`).
    fn install(&mut self, update: &Update, bytes: &[u8]) -> Result<(), String>;
    /// No new engine call from now on (`EngineGate::retire`).
    fn retire(&mut self);
    /// Whether this copy can restart itself at all.
    fn restart_ready(&self) -> bool;
    /// Counted engine work running right now.
    fn busy(&self) -> bool;
    /// Wait for quiet, capped (`EngineGate::wait_quiet`); false = stalled.
    fn wait_quiet(&mut self) -> bool;
    /// Start the new copy (`restart::relaunch`) and close this one.
    fn restart(&mut self) -> Result<(), String>;
}

/// The whole run. `live` is the update a check just returned, or
/// None for a run started from a remembered offer.
pub fn install_and_restart(
    live: Option<Update>,
    steps: &mut dyn Steps,
    emit: &mut dyn FnMut(Event),
) -> Outcome {
    let update = match live {
        Some(u) => u,
        None => match steps.fresh_check() {
            Err(message) => {
                emit(Event::Failed {
                    version: None,
                    message: message.clone(),
                });
                return Outcome::Error {
                    message,
                    update: None,
                };
            }
            Ok(None) => {
                emit(Event::Cleared);
                return Outcome::Current;
            }
            Ok(Some(u)) => u,
        },
    };
    let version = update.version.clone();
    let body = update.body.clone().unwrap_or_default();
    let fail = |message: String, emit: &mut dyn FnMut(Event)| {
        emit(Event::Failed {
            version: Some(version.clone()),
            message: message.clone(),
        });
        Outcome::Error {
            message,
            update: Some(update.clone()),
        }
    };

    emit(Event::Downloading {
        version: version.clone(),
        received: 0,
        total: None,
        body: body.clone(),
    });
    let mut received = 0u64;
    let bytes = {
        // "Installing" follows the download's own end (its finished
        // callback), said once the download call returns, since the
        // progress closure already holds `emit`.
        let mut finished_seen = false;
        let result = steps.download(
            &update,
            &mut |n, total| {
                received += n as u64;
                emit(Event::Downloading {
                    version: version.clone(),
                    received,
                    total,
                    body: body.clone(),
                });
            },
            &mut || finished_seen = true,
        );
        match result {
            Ok(b) => {
                if finished_seen {
                    emit(Event::Installing {
                        version: version.clone(),
                    });
                }
                b
            }
            Err(message) => return fail(message, emit),
        }
    };
    if let Err(message) = steps.install(&update, &bytes) {
        return fail(message, emit);
    }
    // The bundle on disk is the new one now: no new engine call.
    steps.retire();
    if !steps.restart_ready() {
        emit(Event::Installed {
            version: version.clone(),
        });
        return Outcome::Installed { version, update };
    }
    if steps.busy() {
        emit(Event::Waiting {
            version: version.clone(),
        });
    }
    if !steps.wait_quiet() {
        emit(Event::Stalled {
            version: version.clone(),
        });
        return Outcome::Stalled { version, update };
    }
    match restart_installed(&version, steps, emit) {
        RestartOutcome::Restarting => Outcome::Restarting { version, update },
        RestartOutcome::Installed => Outcome::Installed { version, update },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartOutcome {
    Restarting,
    Installed,
}

/// The restart: also what a click on "Restart now" runs. A refused
/// restart is "installed", never a failure: the bundle is already on disk,
/// and "Try again" would reinstall it in a loop.
pub fn restart_installed(
    version: &str,
    steps: &mut dyn Steps,
    emit: &mut dyn FnMut(Event),
) -> RestartOutcome {
    emit(Event::Restarting {
        version: version.into(),
    });
    match steps.restart() {
        Ok(()) => RestartOutcome::Restarting,
        Err(_) => {
            emit(Event::Installed {
                version: version.into(),
            });
            RestartOutcome::Installed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(v: &str) -> Update {
        Update {
            current_version: "0.11.1".into(),
            version: v.into(),
            body: Some("notes".into()),
            date: None,
            target: "darwin-aarch64".into(),
            download_url: "https://x".into(),
            signature: "s".into(),
            raw_json: serde_json::Value::Null,
        }
    }

    #[derive(Default)]
    struct Fake {
        log: Vec<String>,
        fresh: Option<Result<Option<Update>, String>>,
        download_fails: bool,
        install_fails: bool,
        cannot_restart: bool,
        busy: bool,
        never_quiet: bool,
        restart_refused: bool,
    }

    impl Steps for Fake {
        fn fresh_check(&mut self) -> Result<Option<Update>, String> {
            self.log.push("check".into());
            self.fresh.take().unwrap_or(Ok(None))
        }
        fn download(
            &mut self,
            _u: &Update,
            progress: &mut dyn FnMut(usize, Option<u64>),
            finished: &mut dyn FnMut(),
        ) -> Result<Vec<u8>, String> {
            self.log.push("download".into());
            if self.download_fails {
                return Err("signature refused".into());
            }
            progress(40, Some(100));
            progress(60, Some(100));
            finished();
            Ok(vec![1, 2, 3])
        }
        fn install(&mut self, _u: &Update, b: &[u8]) -> Result<(), String> {
            self.log.push(format!("install {}", b.len()));
            if self.install_fails {
                Err("no disk".into())
            } else {
                Ok(())
            }
        }
        fn retire(&mut self) {
            self.log.push("retire".into());
        }
        fn restart_ready(&self) -> bool {
            !self.cannot_restart
        }
        fn busy(&self) -> bool {
            self.busy
        }
        fn wait_quiet(&mut self) -> bool {
            self.log.push("wait".into());
            !self.never_quiet
        }
        fn restart(&mut self) -> Result<(), String> {
            self.log.push("restart".into());
            if self.restart_refused {
                Err("refused".into())
            } else {
                Ok(())
            }
        }
    }

    fn run(f: &mut Fake, live: Option<Update>) -> (Outcome, Vec<Event>) {
        let mut events = Vec::new();
        let out = install_and_restart(live, f, &mut |e| events.push(e));
        (out, events)
    }

    fn v() -> String {
        "0.12.0".into()
    }

    #[test]
    fn the_whole_run_in_order() {
        let mut f = Fake::default();
        let (out, events) = run(&mut f, Some(update("0.12.0")));
        assert!(matches!(out, Outcome::Restarting { .. }));
        assert_eq!(
            f.log,
            ["download", "install 3", "retire", "wait", "restart"]
        );
        assert_eq!(
            events,
            [
                Event::Downloading {
                    version: v(),
                    received: 0,
                    total: None,
                    body: "notes".into()
                },
                Event::Downloading {
                    version: v(),
                    received: 40,
                    total: Some(100),
                    body: "notes".into()
                },
                Event::Downloading {
                    version: v(),
                    received: 100,
                    total: Some(100),
                    body: "notes".into()
                },
                Event::Installing { version: v() },
                Event::Restarting { version: v() },
            ]
        );
    }

    #[test]
    fn busy_says_waiting_and_a_stuck_call_stalls() {
        let mut f = Fake {
            busy: true,
            never_quiet: true,
            ..Default::default()
        };
        let (out, events) = run(&mut f, Some(update("0.12.0")));
        assert!(matches!(out, Outcome::Stalled { .. }));
        assert!(events.contains(&Event::Waiting { version: v() }));
        assert_eq!(events.last(), Some(&Event::Stalled { version: v() }));
        assert!(!f.log.contains(&"restart".to_string()));
        // "Restart now" afterwards:
        let mut later = Vec::new();
        assert_eq!(
            restart_installed("0.12.0", &mut f, &mut |e| later.push(e)),
            RestartOutcome::Restarting
        );
    }

    #[test]
    fn a_refused_restart_is_installed_not_failed() {
        let mut f = Fake {
            restart_refused: true,
            ..Default::default()
        };
        let (out, events) = run(&mut f, Some(update("0.12.0")));
        assert!(matches!(out, Outcome::Installed { .. }));
        assert_eq!(events.last(), Some(&Event::Installed { version: v() }));
    }

    #[test]
    fn a_copy_that_cannot_restart_says_installed_after_retiring() {
        let mut f = Fake {
            cannot_restart: true,
            ..Default::default()
        };
        let (out, _) = run(&mut f, Some(update("0.12.0")));
        assert!(matches!(out, Outcome::Installed { .. }));
        assert_eq!(f.log, ["download", "install 3", "retire"]);
    }

    #[test]
    fn a_failed_download_or_install_never_retires() {
        for f in [
            &mut Fake {
                download_fails: true,
                ..Default::default()
            },
            &mut Fake {
                install_fails: true,
                ..Default::default()
            },
        ] {
            let (out, events) = run(f, Some(update("0.12.0")));
            assert!(matches!(
                out,
                Outcome::Error {
                    update: Some(_),
                    ..
                }
            ));
            assert!(matches!(
                events.last(),
                Some(Event::Failed {
                    version: Some(_),
                    ..
                })
            ));
            assert!(!f.log.contains(&"retire".to_string()));
        }
    }

    #[test]
    fn a_remembered_offer_checks_afresh_first() {
        let mut offline = Fake {
            fresh: Some(Err("offline".into())),
            ..Default::default()
        };
        let (out, events) = run(&mut offline, None);
        assert_eq!(
            out,
            Outcome::Error {
                message: "offline".into(),
                update: None
            }
        );
        assert_eq!(offline.log, ["check"]);
        assert!(matches!(events[0], Event::Failed { version: None, .. }));

        let mut current = Fake::default();
        let (out, events) = run(&mut current, None);
        assert_eq!(out, Outcome::Current);
        assert_eq!(events, [Event::Cleared]);

        let mut newer = Fake {
            fresh: Some(Ok(Some(update("0.12.1")))),
            ..Default::default()
        };
        let (out, _) = run(&mut newer, None);
        assert!(matches!(out, Outcome::Restarting { version, .. } if version == "0.12.1"));
    }
}
