#!/usr/bin/env python3
"""Tests for check-crate-ages.py. Standard library only, no network: every request goes
to a fake urlopen. Run with: python3 scripts/test_check_crate_ages.py"""

import datetime as dt
import email.message
import importlib.util
import io
import json
import pathlib
import tempfile
import unittest
import urllib.error
from contextlib import redirect_stdout

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("check_crate_ages", HERE / "check-crate-ages.py")
cca = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cca)

NOW = dt.datetime(2026, 10, 19, 12, 0, tzinfo=dt.timezone.utc)
SHA = "0123456789abcdef0123456789abcdef01234567"
REPO = "https://github.com/method-production-suite/method-libs"


def ago(days):
    return (NOW - dt.timedelta(days=days)).strftime("%Y-%m-%dT%H:%M:%SZ")


def index_file(*versions):
    """A sparse-index file: (version, days old or None for no pubtime, yanked)."""
    lines = []
    for vers, days, yanked in versions:
        e = {"name": "x", "vers": vers, "deps": [], "cksum": "0" * 64, "features": {},
             "yanked": yanked}
        if days is not None:
            e["pubtime"] = ago(days)
        lines.append(json.dumps(e))
    return "\n".join(lines) + "\n"


class Response(io.BytesIO):
    def __init__(self, body="", headers=None):
        super().__init__(body.encode())
        msg = email.message.Message()
        for k, v in (headers or {}).items():
            msg[k] = v
        self.headers = msg

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


def http_error(url, code, headers=None):
    msg = email.message.Message()
    for k, v in (headers or {}).items():
        msg[k] = v
    return urllib.error.HTTPError(url, code, "error", msg, io.BytesIO())


class FakeNet:
    """urlopen and sleep fakes. routes maps a URL to a list of outcomes, used in turn (the
    last repeats): a Response, or an exception to raise."""

    def __init__(self, routes):
        self.routes = {k: list(v) if isinstance(v, list) else [v] for k, v in routes.items()}
        self.requests = []
        self.slept = []

    def urlopen(self, req, timeout=None):
        url = req.full_url
        self.requests.append((req.get_method(), url))
        if url not in self.routes:
            raise http_error(url, 404)
        outcomes = self.routes[url]
        outcome = outcomes.pop(0) if len(outcomes) > 1 else outcomes[0]
        if isinstance(outcome, BaseException):
            raise outcome
        return outcome

    def sleep(self, seconds):
        self.slept.append(seconds)

    def registry(self):
        return cca.Registry(sleep=self.sleep, urlopen=self.urlopen)


def idx_url(name):
    return f"https://index.crates.io/{cca.index_path(name)}"


def run(pkgs, routes, **kw):
    net = FakeNet(routes)
    out = []
    bad, pins, warnings = cca.check(pkgs, net.registry(), now=NOW, out=out.append, **kw)
    return bad, pins, warnings, out, net


def crate(name, version):
    return (name, version, cca.CRATES_IO)


class IndexParse(unittest.TestCase):
    def test_index_path(self):
        self.assertEqual(cca.index_path("a"), "1/a")
        self.assertEqual(cca.index_path("ab"), "2/ab")
        self.assertEqual(cca.index_path("abc"), "3/a/abc")
        self.assertEqual(cca.index_path("Serde"), "se/rd/serde")

    def test_parse_index(self):
        parsed = cca.parse_index(index_file(("1.0.0", 100, False), ("1.0.1", None, True)))
        self.assertEqual(parsed["1.0.0"]["pubtime"], NOW - dt.timedelta(days=100))
        self.assertFalse(parsed["1.0.0"]["yanked"])
        self.assertIsNone(parsed["1.0.1"]["pubtime"])
        self.assertTrue(parsed["1.0.1"]["yanked"])

    def test_old_crate_passes_from_index_alone(self):
        bad, pins, _, _, net = run(
            {crate("serde", "1.0.200")},
            {idx_url("serde"): Response(index_file(("1.0.200", 60, False)))},
        )
        self.assertEqual(bad, [])
        self.assertEqual(net.requests, [("GET", idx_url("serde"))])

    def test_no_pubtime_uses_static_file_date(self):
        stamp = "Mon, 01 Jan 2024 00:00:00 GMT"
        bad, *_ = run(
            {crate("libc", "0.2.1")},
            {idx_url("libc"): Response(index_file(("0.2.1", None, False))),
             "https://static.crates.io/crates/libc/libc-0.2.1.crate":
                 Response(headers={"Last-Modified": stamp})},
        )
        self.assertEqual(bad, [])

    def test_no_pubtime_and_recent_file_falls_back_to_api(self):
        api = "https://crates.io/api/v1/crates/libc/0.2.1"
        bad, _, _, _, net = run(
            {crate("libc", "0.2.1")},
            {idx_url("libc"): Response(index_file(("0.2.1", None, False))),
             "https://static.crates.io/crates/libc/libc-0.2.1.crate":
                 Response(headers={"Last-Modified": "Sun, 18 Oct 2026 00:00:00 GMT"}),
             api: Response(json.dumps({"version": {"created_at": ago(400), "yanked": False}}))},
        )
        self.assertEqual(bad, [])
        self.assertIn(("GET", api), net.requests)

    def test_yanked_fails(self):
        bad, *_ = run({crate("foo", "1.0.0")},
                      {idx_url("foo"): Response(index_file(("1.0.0", 60, True)))})
        self.assertEqual(bad, ["foo 1.0.0 (yanked)"])


class Cooldown(unittest.TestCase):
    INDEX = index_file(("1.2.0", 90, False), ("1.2.1", 30, False), ("1.2.2", 3, False))

    def test_young_crate_fails(self):
        bad, _, _, out, _ = run({crate("foo", "1.2.2")}, {idx_url("foo"): Response(self.INDEX)})
        self.assertEqual(bad, ["foo 1.2.2"])
        self.assertIn("TOO NEW", out[0])

    def test_age_out_exception_passes_normally(self):
        exc = {("foo", "1.2.2"): "age-out"}
        bad, *_ = run({crate("foo", "1.2.2")}, {idx_url("foo"): Response(self.INDEX)},
                      exceptions=exc)
        self.assertEqual(bad, [])

    def test_age_out_exception_fails_under_release(self):
        exc = {("foo", "1.2.2"): "age-out"}
        bad, *_ = run({crate("foo", "1.2.2")}, {idx_url("foo"): Response(self.INDEX)},
                      exceptions=exc, release=True)
        self.assertEqual(bad, ["foo 1.2.2"])

    def test_security_exception_passes_under_release(self):
        exc = {("foo", "1.2.2"): "security"}
        bad, *_ = run({crate("foo", "1.2.2")}, {idx_url("foo"): Response(self.INDEX)},
                      exceptions=exc, release=True)
        self.assertEqual(bad, [])

    def test_owner_override_holds_for_its_tag_only(self):
        exc = {("foo", "1.2.2"): ("owner-approved", "v1.0.0")}
        def index():
            return {idx_url("foo"): Response(self.INDEX)}
        bad, *_ = run({crate("foo", "1.2.2")}, index(), exceptions=exc)
        self.assertEqual(bad, [])
        bad, *_ = run({crate("foo", "1.2.2")}, index(), exceptions=exc, release=True, tag="v1.0.0")
        self.assertEqual(bad, [])
        for tag in ("v1.0.1", "v0.6.0", None):
            bad, _, _, out, _ = run({crate("foo", "1.2.2")}, index(), exceptions=exc,
                                    release=True, tag=tag)
            self.assertEqual(bad, ["foo 1.2.2"], tag)
            self.assertIn("the owner's override for v1.0.0 only", out[0])

    def test_exception_file_kinds(self):
        with tempfile.TemporaryDirectory() as d:
            path = pathlib.Path(d) / "crate-age-exceptions.txt"
            path.write_text(
                "# comment line\n"
                "rustls 0.23.46   # RUSTSEC-2026-0346 (2026-10-14)\n"
                "parley 0.12.0    # no older compatible release; 2026-10-23\n"
                "clipboard-win 5.4.3  # owner-approved override, the owner, 2026-10-27, release v1.0.0\n"
                "muda 0.21.2      # owner-approved override, the owner, 2026-10-19, no release named\n"
                "malformed\n"
            )
            self.assertEqual(cca.read_exceptions(path), {
                ("rustls", "0.23.46"): "security",
                ("parley", "0.12.0"): "age-out",
                ("clipboard-win", "5.4.3"): ("owner-approved", "v1.0.0"),
                ("muda", "0.21.2"): "age-out",
            })
        self.assertEqual(cca.read_exceptions(pathlib.Path("/nonexistent/file")), {})

    def test_aged_exception_warns(self):
        exc = {("foo", "1.2.1"): "age-out", ("gone", "1.0.0"): "security"}
        bad, _, warnings, *_ = run({crate("foo", "1.2.1")},
                                   {idx_url("foo"): Response(self.INDEX)}, exceptions=exc)
        self.assertEqual(bad, [])
        self.assertEqual(warnings, ["foo 1.2.1 is 14+ days old now", "gone 1.0.0 is not in the lock"])

    def test_only_changed_does_not_warn_about_unlocked_entries(self):
        exc = {("gone", "1.0.0"): "security"}
        _, _, warnings, *_ = run({crate("foo", "1.2.1")},
                                 {idx_url("foo"): Response(self.INDEX)},
                                 exceptions=exc, full=False)
        self.assertEqual(warnings, [])


class GitAllowlist(unittest.TestCase):
    def test_pinned_and_listed_passes(self):
        src = f"git+{REPO}?rev={SHA}#{SHA}"
        bad, *_ = run({("method-core", "0.1.0", src)}, {}, git_allow={REPO})
        self.assertEqual(bad, [])

    def test_unpinned_fails(self):
        for src in (f"git+{REPO}?tag=v0.1.0#{SHA}", f"git+{REPO}?branch=main#{SHA}",
                    f"git+{REPO}#{SHA}", f"git+{REPO}?rev={SHA[:12]}#{SHA}"):
            bad, *_ = run({("method-core", "0.1.0", src)}, {}, git_allow={REPO})
            self.assertEqual(len(bad), 1, src)

    def test_unlisted_fails(self):
        src = f"git+https://github.com/someone/else?rev={SHA}#{SHA}"
        bad, *_ = run({("other", "0.1.0", src)}, {}, git_allow={REPO})
        self.assertEqual(len(bad), 1)

    def test_other_registry_fails(self):
        bad, *_ = run({("x", "1.0.0", "registry+https://example.invalid/index")}, {},
                      git_allow={REPO})
        self.assertEqual(len(bad), 1)

    def test_allow_file(self):
        with tempfile.TemporaryDirectory() as d:
            path = pathlib.Path(d) / "crate-age-git-allow.txt"
            path.write_text(f"# reviewed repositories\n{REPO}   # shared crates\n\n")
            self.assertEqual(cca.read_git_allow(path), {REPO})


class Retry(unittest.TestCase):
    def test_retry_wait_schedule(self):
        net_err = urllib.error.URLError("connection reset")
        self.assertEqual([cca.retry_wait(net_err, i) for i in range(8)],
                         [5, 15, 30, 60, 120, 300, 300, None])
        self.assertEqual(cca.retry_wait(TimeoutError(), 0), 5)
        self.assertEqual(cca.retry_wait(ConnectionError(), 1), 15)
        self.assertIsNone(cca.retry_wait(http_error("u", 404), 0))
        self.assertEqual(cca.retry_wait(http_error("u", 503), 2), 30)
        self.assertEqual(cca.retry_wait(http_error("u", 429, {"Retry-After": "42"}), 0), 42)
        self.assertEqual(cca.retry_wait(http_error("u", 429, {"Retry-After": "9999"}), 0), 300)

    def test_fetch_retries_then_succeeds(self):
        url = idx_url("foo")
        net = FakeNet({url: [urllib.error.URLError("dns"), TimeoutError(), ConnectionError(),
                             http_error(url, 429, {"Retry-After": "7"}), http_error(url, 502),
                             Response(index_file(("1.0.0", 60, False)))]})
        text, _ = cca.fetch(url, sleep=net.sleep, urlopen=net.urlopen)
        self.assertIn("1.0.0", text)
        self.assertEqual(net.slept, [5, 15, 30, 7, 120])

    def test_fetch_gives_up(self):
        url = idx_url("foo")
        net = FakeNet({url: [ConnectionError()]})
        with self.assertRaises(ConnectionError):
            cca.fetch(url, sleep=net.sleep, urlopen=net.urlopen)
        self.assertEqual(net.slept, [5, 15, 30, 60, 120, 300, 300])

    def test_fetch_does_not_retry_404(self):
        url = idx_url("foo")
        net = FakeNet({})
        with self.assertRaises(urllib.error.HTTPError):
            cca.fetch(url, sleep=net.sleep, urlopen=net.urlopen)
        self.assertEqual(net.slept, [])

    def test_check_survives_a_rate_limit(self):
        url = idx_url("foo")
        bad, _, _, _, net = run({crate("foo", "1.0.0")},
                                {url: [http_error(url, 429), Response(index_file(("1.0.0", 60, False)))]})
        self.assertEqual(bad, [])
        self.assertEqual(net.slept, [5])


class Precise(unittest.TestCase):
    def test_suggests_newest_compatible_old_unyanked(self):
        index = index_file(
            ("1.3.0", 100, False),  # older but caret-compatible: not the newest candidate
            ("1.4.0", 40, False),   # the answer
            ("1.4.1", 30, True),    # yanked
            ("1.4.2", 10, False),   # too young
            ("1.4.3", 2, False),    # locked
            ("2.0.0", 90, False),   # incompatible
        )
        bad, pins, _, out, _ = run({crate("foo", "1.4.3")}, {idx_url("foo"): Response(index)})
        self.assertEqual(bad, ["foo 1.4.3"])
        self.assertEqual(pins, ["cargo update -p foo@1.4.3 --precise 1.4.0"])
        self.assertIn("pin to 1.4.0", out[0])

    def test_zero_major_caret(self):
        index = index_file(("0.2.9", 100, False), ("0.3.0", 100, False), ("0.3.1", 1, False))
        _, pins, *_ = run({crate("foo", "0.3.1")}, {idx_url("foo"): Response(index)})
        self.assertEqual(pins, ["cargo update -p foo@0.3.1 --precise 0.3.0"])

    def test_no_candidate(self):
        index = index_file(("0.2.9", 100, False), ("0.3.0", 1, False))
        bad, pins, _, out, _ = run({crate("foo", "0.3.0")}, {idx_url("foo"): Response(index)})
        self.assertEqual(bad, ["foo 0.3.0"])
        self.assertEqual(pins, [])
        self.assertIn("no older compatible version", out[0])

    def test_compatible(self):
        self.assertTrue(cca.compatible((1, 2, 3), (1, 9, 0)))
        self.assertFalse(cca.compatible((1, 0, 0), (2, 0, 0)))
        self.assertTrue(cca.compatible((0, 3, 0), (0, 3, 7)))
        self.assertFalse(cca.compatible((0, 3, 0), (0, 4, 0)))
        self.assertFalse(cca.compatible((0, 0, 1), (0, 0, 2)))
        self.assertIsNone(cca.semver("1.0.0-beta.1"))
        self.assertEqual(cca.semver("0.1.0+spec-1.1.0"), (0, 1, 0))


class Main(unittest.TestCase):
    LOCK = f"""version = 4

[[package]]
name = "app"
version = "0.1.0"

[[package]]
name = "foo"
version = "1.2.2"
source = "{cca.CRATES_IO}"

[[package]]
name = "method-core"
version = "0.1.0"
source = "git+{REPO}?rev={SHA}#{SHA}"
"""

    def main(self, *flags, exceptions=""):
        with tempfile.TemporaryDirectory() as d:
            d = pathlib.Path(d)
            (d / "Cargo.lock").write_text(self.LOCK)
            (d / "exc.txt").write_text(exceptions)
            (d / "allow.txt").write_text(REPO + "\n")
            net = FakeNet({idx_url("foo"): Response(
                index_file(("1.2.1", 30, False), ("1.2.2", 3, False)))})
            buf = io.StringIO()
            with redirect_stdout(buf):
                code = cca.main(["--lock", str(d / "Cargo.lock"), "--exceptions", str(d / "exc.txt"),
                                 "--git-allow", str(d / "allow.txt"), *flags],
                                registry=net.registry(), now=NOW)
            return code, buf.getvalue()

    def test_fails_with_suggestion(self):
        code, out = self.main()
        self.assertEqual(code, 1)
        self.assertIn("cargo update -p foo@1.2.2 --precise 1.2.1", out)
        self.assertIn("pinned git commit, allowlisted repository", out)

    def test_age_out_and_release(self):
        exc = "foo 1.2.2   # waits for a dependency; 2026-10-30\n"
        self.assertEqual(self.main(exceptions=exc)[0], 0)
        self.assertEqual(self.main("--release", exceptions=exc)[0], 1)

    def test_owner_override_and_release(self):
        exc = "foo 1.2.2   # owner-approved override, the owner, 2026-10-27, release v1.0.0\n"
        self.assertEqual(self.main("--release", "--tag", "v1.0.0", exceptions=exc)[0], 0)
        self.assertEqual(self.main("--release", "--tag", "v1.0.1", exceptions=exc)[0], 1)
        self.assertEqual(self.main("--release", exceptions=exc)[0], 1)
        unnamed = "foo 1.2.2   # owner-approved override, the owner, 2026-10-27\n"
        self.assertEqual(self.main("--release", "--tag", "v1.0.0", exceptions=unnamed)[0], 1)

    def test_owner_override_names_the_whole_tag(self):
        for other in ("v1.0.0-rc1", "v1.0.0.1", "v1.0.0+build", "v1.0.0x"):
            exc = f"foo 1.2.2   # owner-approved override, the owner, 2026-10-27, release {other}\n"
            self.assertEqual(self.main("--release", "--tag", "v1.0.0", exceptions=exc)[0], 1, other)
        exc = "foo 1.2.2   # owner-approved override, the owner, 2026-10-27, release v1.0.0; remove on 2026-11-06\n"
        self.assertEqual(self.main("--release", "--tag", "v1.0.0", exceptions=exc)[0], 0)

    def test_security_and_release(self):
        exc = "foo 1.2.2   # RUSTSEC-2026-0001\n"
        self.assertEqual(self.main("--release", exceptions=exc)[0], 0)


if __name__ == "__main__":
    unittest.main()
