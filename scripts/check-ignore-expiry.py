#!/usr/bin/env python3
"""Expiry guard for temporary advisory ignores in deny.toml.

Usage: python3 scripts/check-ignore-expiry.py [--deny deny.toml] [--today YYYY-MM-DD]

An advisory ignored only until a fixed crate is old enough for the cooldown must not
outlive that date. deny.toml carries the date once, in a comment line
`# ignore-expires: YYYY-MM-DD` above `[advisories] ignore`. After that date this check
fails while any of the advisories it guards is still ignored, so the bump is not
forgotten. With no expiry line it passes. Stdlib only, so it runs anywhere Python 3 does.
"""

import argparse
import datetime as dt
import re
import sys
import tomllib

# The ignores the expiry date covers: rustls advisories fixed in 0.23.46.
GUARDED = {f"RUSTSEC-2026-{n:04d}" for n in range(346, 350)}

EXPIRY = re.compile(r"^\s*#\s*ignore-expires:\s*(\d{4}-\d{2}-\d{2})\s*$", re.MULTILINE)


def ignored_ids(deny):
    """Every advisory id in [advisories] ignore, as a string or {id = ...}."""
    ids = set()
    for entry in deny.get("advisories", {}).get("ignore", []):
        ids.add(entry["id"] if isinstance(entry, dict) else entry)
    return ids


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--deny", default="deny.toml")
    ap.add_argument("--today", type=dt.date.fromisoformat, default=dt.date.today())
    args = ap.parse_args()

    with open(args.deny) as f:
        text = f.read()
    still = sorted(ignored_ids(tomllib.loads(text)) & GUARDED)
    dates = EXPIRY.findall(text)
    if not dates:
        if still:
            print(f"{args.deny} ignores {', '.join(still)} but has no `# ignore-expires:` line")
            sys.exit(1)
        print("no expiring ignores")
        return
    if len(dates) > 1:
        print(f"{args.deny} has {len(dates)} `# ignore-expires:` lines; keep the date in one place")
        sys.exit(1)
    expires = dt.date.fromisoformat(dates[0])
    if args.today > expires and still:
        print(f"expired on {expires}: {args.deny} still ignores {', '.join(still)}")
        print("bump the fixed crate (see the comment in deny.toml) and delete the ignores")
        sys.exit(1)
    if not still:
        print(f"none of the guarded ignores is left; the {expires} expiry line can go")
        return
    left = (expires - args.today).days
    print(f"ignores {', '.join(still)} until {expires} ({left} days left)")


if __name__ == "__main__":
    main()
