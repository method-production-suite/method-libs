//! The Method Production Suite's updater, shared by every app that does not
//! use its UI framework's own: fetch `latest.json` (Tauri's static manifest
//! format, which every suite release pipeline already publishes), verify
//! the minisign signature of the download against the one public key the
//! app was built with, swap the Mac bundle in place, restart into it, and
//! clean up after a swap that was interrupted.
//!
//! Nothing here knows which app it serves. Each app builds one
//! [`AppConfig`] (its name, bundle name, user agent, endpoint, key and the
//! sentences it shows) and hands it to the functions below. The
//! [`policy`] module adds the suite's update behaviour on top: a first
//! check 2 minutes after launch, then one every 6 hours, a switch for
//! automatic checks that is on by default, the settings file that keeps it,
//! and the state machine a window draws. It is driven by the app's own
//! timer or thread, so any UI framework can use it.
//!
//! Updates are for Apple silicon Macs only (SUITE-STANDARDS.md, section 1):
//! the manifest key is `darwin-aarch64`. The crate still builds and tests
//! on every desktop OS, because the apps do.
//!
//! No UI framework, no async runtime. HTTPS through ureq and rustls, and
//! only to the endpoint's host and GitHub's two download hosts, redirects
//! included ([`transport`]).

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod config;
pub mod flow;
pub mod gate;
pub mod install;
pub mod manifest;
pub mod policy;
pub mod restart;
pub mod target;
pub mod transport;
pub mod verify;

mod fsutil;

pub use config::{AppConfig, Baked, BundleNames, GITHUB_DOWNLOAD_HOSTS, Messages};
pub use install::{InstallError, Installed, Swept};
pub use manifest::{Platform, Release};
pub use policy::{Phase, Said, Settings, Status, Updater};
pub use target::{Installer, TargetSet};
pub use transport::{Config, Update, UpdateError, check, download};
