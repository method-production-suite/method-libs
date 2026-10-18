# method-update

The Method Production Suite's updater: one crate every suite app can use to check for,
download, verify and install a new version of itself on an Apple silicon Mac, with the
suite's update behaviour built in.

- **Manifest**: `latest.json` in Tauri's static format, the file every suite release
  pipeline already publishes. The key it reads is `darwin-aarch64` (after
  `darwin-aarch64-app`). Any other machine is told updates are for Apple silicon Macs
  only.
- **Signature**: minisign, against the one public key the app was built with. The
  signature's trusted comment must name the version the manifest announces.
- **Network**: https only, and only to the endpoint's host, GitHub's two download hosts
  (`objects.githubusercontent.com`, `release-assets.githubusercontent.com`) when the
  endpoint is on github.com, and any host the app adds. Redirects are followed one hop at
  a time, and every hop meets the same rule.
- **Install**: the `.app` bundle is swapped in place. Staging and backup folders sit
  beside the bundle, on the same volume. An archive entry with `..`, an absolute path or a
  link out of the bundle is refused. The unpacked bundle must have an Info.plist whose
  `CFBundleExecutable` exists before anything moves. A failure after the running bundle
  moved puts it back. A backup that can't be deleted is reported as `leftover`, not as a
  failure. When /Applications isn't writable, an administrator does the swap through
  osascript. An app that was renamed can name its old bundle, and the next update moves
  it to the new name, once.
- **Restart**: starts `Installed::bundle` (the new path after a rename), reading
  `CFBundleExecutable` from the NEW Info.plist.
- **Startup sweep**: puts back a bundle left hidden by an interrupted swap
  (`.<name>.update-<pid>`, `.<name>.old-<pid>`), and removes leftovers older than ten
  minutes, but never the bundle the app is running from.
- **Policy** (`method_update::policy`): first check 2 minutes after launch, then every 6
  hours; a "Check for Updates Automatically" switch, on by default, kept in a small JSON
  file the app names (written atomically); the state machine a window draws (idle,
  checking, downloading, ready, installing, failed). An automatic check that fails says
  nothing; a check the person asked for always answers.

No UI framework, no async runtime, no threads of its own: the app's timer or thread
drives it. The updater never reads or checks the bundle identifier.

## Wiring an app

Each app describes itself once:

```rust
use method_update::{AppConfig, baked};

/// Where this app looks for a new version, and the key its releases are signed with.
const ENDPOINT: &str =
    "https://github.com/method-production-suite/<repo>/releases/latest/download/latest.json";
const PUBKEY: &str = "<base64 of the minisign public key box>";

pub fn update_config() -> AppConfig {
    AppConfig::new(
        "Method Example",
        "Method Example.app",
        concat!("method-example-update/", env!("CARGO_PKG_VERSION")),
        // A rehearsal build (the app's own `rehearsal` feature) may bake a local
        // endpoint, a throwaway key and plain HTTP from METHOD_EXAMPLE_UPDATE_ENDPOINT,
        // _PUBKEY and _INSECURE at compile time. Any other build ignores them.
        baked!(
            prefix = "METHOD_EXAMPLE_UPDATE",
            rehearsal = cfg!(feature = "rehearsal"),
            endpoint = ENDPOINT,
            pubkey = PUBKEY,
        ),
    )
    // Only for an app whose bundle had another name before.
    // .with_legacy_bundle_name("Old Example.app")
}
```

And refuses the rehearsal variables in any other build, in its `build.rs`:

```rust
fn main() {
    let rehearsal = std::env::var_os("CARGO_FEATURE_REHEARSAL").is_some();
    for suffix in ["_ENDPOINT", "_PUBKEY", "_INSECURE"] {
        let name = format!("METHOD_EXAMPLE_UPDATE{suffix}");
        println!("cargo:rerun-if-env-changed={name}");
        if !rehearsal && std::env::var_os(&name).is_some() {
            panic!("{name} is set, but this is not a rehearsal build.");
        }
    }
}
```

At launch, before the window opens:

```rust
if let Ok(exe) = std::env::current_exe() {
    let swept = method_update::install::sweep_at_startup(&exe, &update_config().bundle);
    // swept.restored / swept.removed: worth a log line, nothing more.
}
```

### An egui-style app

```rust
use std::sync::Arc;
use std::time::Instant;
use method_update::{Updater, policy::TICK};

let updater = Arc::new(Updater::new(
    update_config(),
    env!("CARGO_PKG_VERSION"),
    Some(data_dir.join("updates.json")),
    Instant::now(),
));

// The app's own timer: a thread that looks now and then.
let u = updater.clone();
let ctx = egui_ctx.clone();
std::thread::spawn(move || loop {
    std::thread::sleep(TICK);
    if u.due(Instant::now()) {
        u.check_now(false); // blocks; silent if it fails
        ctx.request_repaint();
    }
});

// Help > Check for Updates...: run it off the UI thread, then show the answer.
let said = updater.check_now(true); // Some(Said::Current) / Some(Said::Failed) / None

// Help > Check for Updates Automatically:
updater.set_automatic(checked)?;

// Each frame: draw updater.status(said). On "ready", offer "Restart to update":
let bundle = method_update::install::bundle_of_exe(&std::env::current_exe()?);
if let Some(bundle) = bundle {
    let names = updater.app().bundle.clone();
    match updater.install_ready(&bundle, &|b, new| {
        method_update::install::admin_swap(b, new, &names)
    }) {
        Ok(installed) => {
            // Save what needs saving first, then:
            let _ = method_update::restart::relaunch(&installed.bundle);
            std::process::exit(0);
        }
        Err(e) => { /* still ready; say e and offer to try again */ }
    }
}

// Once after an update:
if updater.take_whats_new() { /* show What's New */ }
```

An app that shows each moment of the install (download progress, waiting for work in
progress to finish, "Restart now") can use `method_update::flow` with
`method_update::gate::EngineGate` instead of `Updater::install_ready`.

### A Tauri app

The window keeps its own menu items, commands and events; only the work underneath moves
here. `Status` serializes to the same shape the page already reads (`phase`, `version`,
`manual`, `said`, `automatic`, plus `available` and `message`).

```rust
app.manage(Arc::new(Updater::new(
    update_config(),
    &app.package_info().version.to_string(),
    app.path().app_local_data_dir().ok().map(|d| d.join("updates.json")),
    Instant::now(),
)));

// The timer, on a thread of its own:
let handle = app.handle().clone();
std::thread::spawn(move || loop {
    std::thread::sleep(method_update::policy::TICK);
    let u = handle.state::<Arc<Updater>>();
    if u.due(Instant::now()) {
        u.check_now(false);
        let _ = handle.emit("update-status", u.status(None));
    }
});

#[tauri::command]
fn update_status(u: State<'_, Arc<Updater>>) -> method_update::Status {
    u.status(None)
}

// Help > Check for Updates...: tauri::async_runtime::spawn_blocking, then
// emit u.status(u.check_now(true)).
// Restart to update: install_ready(...), then method_update::restart::relaunch(
// &installed.bundle) and app.exit(0).
```

## Migrating

### Method Reader (from `sp-update`)

`method-update` is `sp-update` with the Reader-only parts lifted out into `AppConfig`.

| `sp-update` | `method-update` |
|---|---|
| `Config::method_reader(Some(Installer::App))` | `update_config().transport(Some(Installer::App))` |
| `ENDPOINT`, `PUBKEY`, `baked_endpoint()`, `baked_pubkey()`, `baked_insecure()`, `REHEARSAL` | Reader's own constants, through `baked!(prefix = "METHOD_READER_UPDATE", rehearsal = cfg!(feature = "rehearsal"), ...)` |
| `USER_AGENT` | `AppConfig::user_agent` (`concat!("method-reader-update/", env!("CARGO_PKG_VERSION"))`) |
| `install::BUNDLE_NAME`, `LEGACY_BUNDLE_NAME` | `AppConfig::new(.., "Method Reader.app", ..).with_legacy_bundle_name(LEGACY_BUNDLE_NAME)` |
| `install_app(bytes, &bundle, &admin)` | `install_app(bytes, &bundle, &config.bundle, &admin)` |
| `admin_swap(bundle, new)` | `admin_swap(bundle, new, &config.bundle)` |
| `install_destination(bundle)` | `install_destination(bundle, &config.bundle)` |
| `sweep_at_startup(&exe)` | `sweep_at_startup(&exe, &config.bundle)` |
| `sweep_beside(dir, name, pid)` | `sweep_beside(dir, name, &config.bundle, pid, running)` |
| `gate::EngineGate::new()`, `UPDATED_MESSAGE` | `EngineGate::new(&config.messages.updated)`; a retired `enter` returns `Retired`, whose `Display` is that sentence |
| `UpdateError::TooLarge` | `UpdateError::TooLarge(String)`, the app's `messages.too_large` |
| `flow`, `manifest`, `verify`, `restart`, `transport::{check, download}` | the same |

Stays in Reader, because only Reader needs it: `IDENTIFIER`, `LEGACY_IDENTIFIER`,
`LEGACY_TAURI_IDENTIFIER`, `LEGACY_ENDPOINT`, `webkit.rs` and `sqlite.rs` (the one-time
import of the old window's storage), and the tests `tauri_conf.rs` and `webkit.rs`.
Reader's `build.rs` guard stays with Reader too, as in the snippet above.

Gone: `install_appimage`, `nsis_update_args`, `updater_os`, `updater_arch` and every
`Installer` but `App`: the suite updates Apple silicon Macs only.

New for Reader: the host rule (every request and redirect is checked), the startup sweep
never deletes the bundle it runs from, and the `policy` module, which replaces Reader's
opt-in prompt with the suite's automatic checks. A person who said no to the old prompt
keeps checks off: on the first run with `method-update`, write
`Settings { automatic: false, .. }` to the settings file for them.

### Method Budget and Method Schedule (from `tauri-plugin-updater`)

Keep the page, the Help menu items, the commands (`update_status`, `update_restart`,
`whats_new`) and the `update-status` event. Underneath:

- `Machine`, `Phase`, `Said`, `Start`, `Status`, `UpdateSettings`, `whats_new_due`,
  `allowed_hosts`, `unavailable_reason`, `FIRST_CHECK` and `CHECK_EVERY` come from
  `method_update::policy` and `AppConfig`. `updates.json` keeps its shape
  (`automatic`, `seen`), so existing settings carry over.
- The endpoint and public key move from `plugins.updater` in `tauri.conf.json` into
  `AppConfig` (baked constants), and `tauri-plugin-updater` is removed.
- `check_now` becomes `Updater::check_now` on a blocking thread; the redirect policy is
  built in.
- Restart to update becomes `Updater::install_ready` (after the app's own save-first
  step), then `restart::relaunch(&installed.bundle)` and `app.exit(0)`. A failed install
  leaves the update ready, as before.
- The release's `latest.json` must carry a `darwin-aarch64` entry signed with the
  version in its trusted comment.
