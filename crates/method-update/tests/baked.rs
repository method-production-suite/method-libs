//! The updater values a build bakes in, per app. Only a rehearsal build
//! reads the app's `<PREFIX>_ENDPOINT`, `<PREFIX>_PUBKEY` and
//! `<PREFIX>_INSECURE` compile-time values; every other build has the
//! release endpoint, the release key and HTTPS, whatever was set. The macro
//! expands in the app's crate, so each app reads its own variables.

use method_update::{AppConfig, Baked, baked};

const ENDPOINT: &str = "https://github.com/example/app/releases/latest/download/latest.json";
const PUBKEY: &str = "dW50cnVzdGVk";

#[test]
fn a_release_build_bakes_the_release_values() {
    // Nothing sets these variables while the tests compile; a release build
    // would ignore them if anything did.
    let b = baked!(
        prefix = "METHOD_UPDATE_TEST_APP_UPDATE",
        rehearsal = false,
        endpoint = ENDPOINT,
        pubkey = PUBKEY,
    );
    assert_eq!(b, Baked::release(ENDPOINT, PUBKEY));
    let app = AppConfig::new(
        "Method Example",
        "Method Example.app",
        "method-example-update/1",
        b,
    );
    assert_eq!(app.endpoint, ENDPOINT);
    assert_eq!(app.pubkey, PUBKEY);
    assert!(!app.allow_insecure);
}

#[test]
fn a_rehearsal_with_nothing_set_still_bakes_the_release_values() {
    let b = baked!(
        prefix = "METHOD_UPDATE_TEST_APP_UPDATE",
        rehearsal = true,
        endpoint = ENDPOINT,
        pubkey = PUBKEY,
    );
    assert_eq!(
        (b.endpoint, b.pubkey, b.insecure),
        (ENDPOINT, PUBKEY, false)
    );
    assert!(b.rehearsal);
}

#[test]
fn a_rehearsal_takes_its_local_endpoint_key_and_plain_http() {
    let b = Baked::resolve(
        true,
        ENDPOINT,
        PUBKEY,
        Some("http://127.0.0.1:8123/latest.json"),
        Some("dGhyb3dhd2F5"),
        Some("1"),
    );
    let app = AppConfig::new("Method Example", "Method Example.app", "m/1", b);
    assert_eq!(app.endpoint, "http://127.0.0.1:8123/latest.json");
    assert_eq!(app.pubkey, "dGhyb3dhd2F5");
    assert!(app.allow_insecure);
    assert_eq!(app.allowed_hosts(), ["127.0.0.1"]);
}

#[test]
fn a_release_build_refuses_every_rehearsal_value() {
    let b = Baked::resolve(
        false,
        ENDPOINT,
        PUBKEY,
        Some("http://127.0.0.1:8123/latest.json"),
        Some("dGhyb3dhd2F5"),
        Some("1"),
    );
    assert_eq!(b, Baked::release(ENDPOINT, PUBKEY));
}

#[test]
fn the_variable_names_are_the_prefix_and_three_suffixes() {
    assert_eq!(
        method_update::config::REHEARSAL_VARIABLES,
        ["_ENDPOINT", "_PUBKEY", "_INSECURE"]
    );
}
