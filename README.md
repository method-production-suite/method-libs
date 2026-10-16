# method-libs

Shared Rust libraries for the Method Production Suite apps (Method Storyteller, Method
Reader, Method Schedule and Method Budget), in the spirit of ArtCraft's `craft-libs`.

Empty for now. Planned, in order:

1. `method-update`: the suite's updater for apps that don't use Tauri's (Method Reader
   and Method Storyteller). It grows out of Method Reader's `sp-update`: Tauri's
   `latest.json` format, minisign signatures, checks 2 minutes after launch and every 6
   hours, with a switch to turn checking off. See the suite's update standard.
2. The PDF-to-script crates (model, PDF reading, parsing, Fountain and .fdx output) from
   Method Reader, shared with Method Storyteller's Import PDF and Method Schedule.

## Rules

- One crate per library under `crates/`; apps depend on a tagged release (`git` + `tag`).
- Rust 1.96.1 (`rust-toolchain.toml`), edition 2024, `cargo fmt`, `cargo clippy -D warnings`.
- `deny.toml` (advisories, licenses, sources) and a 14-day minimum age for every new
  crate from crates.io (`scripts/check-crate-ages.py`) run in CI.
- MIT OR Apache-2.0. No GPL code: Method Reader's AZW3 writer stays in that app.
