#!/usr/bin/env python3
"""Cut a release: bump the version, tag it, build it, publish it.

One script because these steps have drifted apart before. Cargo.toml once said
0.1.0 while v0.1.2 was released, which was merely untidy until the application
started comparing its own version against the tags to decide whether it is out
of date. An application whose version lags the tags believes it is permanently
behind and offers an update to something it is already running.

So the bump, the commit and the tag happen together or not at all, and the
binary that gets uploaded is built from the tagged tree rather than from
whatever happened to be lying in target/.

    python3 tools/release.py 0.1.16
    python3 tools/release.py --patch          # 0.1.15 -> 0.1.16
    python3 tools/release.py --patch --dry-run

Requires `gh` for the publish step, and the same lld/cargo-xwin setup the
README describes for the cross-build.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CARGO_TOML = ROOT / "Cargo.toml"
TARGET = "x86_64-pc-windows-msvc"
BUILT = ROOT / "target" / TARGET / "release" / "winsend.exe"
# The name the updater looks for in a release's assets. Renaming it here means
# every installed copy stops finding updates, so it is not a free choice.
ASSET = "winsend.exe"

VERSION = re.compile(r'^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"\s*$', re.MULTILINE)


def run(command: list[str], **kwargs) -> str:
    result = subprocess.run(command, cwd=ROOT, text=True, capture_output=True, **kwargs)
    if result.returncode != 0:
        sys.exit(f"failed: {' '.join(command)}\n{result.stdout}{result.stderr}")
    return result.stdout.strip()


def current_version() -> tuple[int, int, int]:
    match = VERSION.search(CARGO_TOML.read_text())
    if not match:
        sys.exit("could not find a three-number version in Cargo.toml")
    return tuple(int(part) for part in match.groups())


def write_version(version: tuple[int, int, int]) -> None:
    text = CARGO_TOML.read_text()
    bumped = VERSION.sub(f'version = "{".".join(map(str, version))}"', text, count=1)
    if bumped == text:
        sys.exit("Cargo.toml was not changed, which means the pattern stopped matching")
    CARGO_TOML.write_text(bumped)


def parse(text: str) -> tuple[int, int, int]:
    parts = text.lstrip("v").split(".")
    if len(parts) != 3 or not all(part.isdigit() for part in parts):
        sys.exit(f"{text!r} is not a three-number version; the updater will not parse it")
    return tuple(int(part) for part in parts)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", nargs="?", help="the new version, e.g. 0.1.16")
    parser.add_argument("--patch", action="store_true", help="bump the last number")
    parser.add_argument("--notes", default="", help="one line describing the release")
    parser.add_argument("--dry-run", action="store_true", help="say what would happen")
    args = parser.parse_args()

    current = current_version()
    if args.patch and not args.version:
        new = (current[0], current[1], current[2] + 1)
    elif args.version and not args.patch:
        new = parse(args.version)
    else:
        parser.error("give a version or --patch, not both and not neither")

    if new <= current:
        sys.exit(
            f"{'.'.join(map(str, new))} is not above the current "
            f"{'.'.join(map(str, current))}. An installed copy compares numerically, "
            "so it would never offer this."
        )

    tag = "v" + ".".join(map(str, new))

    # Everything that can refuse, before anything that changes state.
    if run(["git", "status", "--porcelain"]):
        sys.exit("the working tree has changes; commit or stash them first")
    if run(["git", "tag", "--list", tag]):
        sys.exit(f"{tag} already exists")

    print(f"{'.'.join(map(str, current))} -> {'.'.join(map(str, new))} ({tag})")
    if args.dry_run:
        print("dry run: nothing was changed")
        return

    write_version(new)
    # Refreshes Cargo.lock, which records the crate's own version, so the two
    # are committed together rather than the lock drifting a release behind.
    run(["cargo", "check", "--quiet"])

    run(["git", "add", "Cargo.toml", "Cargo.lock"])
    run(["git", "commit", "-m", f"chore: bump the version ahead of {tag}"])
    # Only after the bump is committed, which is the whole point of this
    # script: the tag can never point at a tree that misreports its version.
    run(["git", "tag", tag])

    print("building for Windows")
    lld = run(["brew", "--prefix", "lld"]) + "/bin"
    environment = dict(os.environ, PATH=lld + ":" + os.environ["PATH"])
    subprocess.run(
        ["cargo", "xwin", "build", "--release", "--target", TARGET, "--bin", "winsend"],
        cwd=ROOT,
        env=environment,
        check=True,
    )
    if not BUILT.exists():
        sys.exit(f"the build reported success but {BUILT} is not there")
    if BUILT.name != ASSET:
        sys.exit(f"the build produced {BUILT.name}, and the updater looks for {ASSET}")

    # Before publishing, and this order is not incidental. `gh release create`
    # creates the tag remotely if it is not already there, and it creates it at
    # the default branch's head — which would be the commit before the bump.
    # The release would then point at a tree whose Cargo.toml reports the
    # previous version, which is the exact drift this script exists to prevent.
    print("pushing the bump and the tag")
    run(["git", "push", "origin", "HEAD"])
    run(["git", "push", "origin", tag])

    print(f"publishing {tag}")
    run(
        [
            "gh",
            "release",
            "create",
            tag,
            str(BUILT),
            "--title",
            f"{tag} — {args.notes}" if args.notes else tag,
            "--notes",
            args.notes or "See the commits since the previous tag.",
            "--prerelease",
        ]
    )

    print(f"released {tag}")


if __name__ == "__main__":
    main()
