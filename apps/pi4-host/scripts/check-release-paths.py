#!/usr/bin/env python3
"""Fail when a Release workflow path filter misses an input of the release build.

PAS-2348: helper and embedded-unit changes merged without running Release because
.github/workflows/release.yml only listed manifests and scripts. Every path the
Docker build streams into the builder (build-release.sh) must be covered by both
the push and pull_request filters.

usage: check-release-paths.py [REPO_ROOT]
"""
import fnmatch
import re
import sys
from pathlib import Path

root = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).resolve().parents[3])
build = (root / "apps/pi4-host/scripts/build-release.sh").read_text()
workflow = (root / ".github/workflows/release.yml").read_text()

# The `tar -C "$WORKSPACE" -cf - \ ... \` source slice up to the docker pipe.
m = re.search(r'tar -C "\$WORKSPACE" -cf - \\\n(.*?)\n\s*\| docker run', build, re.S)
if not m:
    sys.exit("error: source slice (tar -C \"$WORKSPACE\" -cf - ...) not found in build-release.sh")
inputs = [w.strip('"') for w in m.group(1).replace("\\\n", " ").split()]
# Generated during the build, not tracked: notices come from manifests and lockfiles.
inputs = [p for p in inputs if p and "$" not in p and p != "\\"]
inputs = ["apps/web/src" if p == "apps/web/dist" else p for p in inputs]
inputs += ["apps/web/bun.lock", "apps/web/package.json", "mise.toml", "apps/pi4-host/scripts/build-release.sh"]
# The CLI is a workspace member only so the root lockfile resolves; its sources are not
# compiled into vinyl-bridge-pi, so only its manifest has to trigger Release.
inputs = ["apps/airplay-rs/cli/Cargo.toml" if p == "apps/airplay-rs/cli" else p for p in inputs]

blocks = re.findall(r"^  (push|pull_request):\n(.*?)(?=^  \S|\Z)", workflow, re.S | re.M)
filters = {}
for event, body in blocks:
    p = re.search(r"^    paths:\n((?:      .*\n)+)", body, re.M)
    filters[event] = [l.split("- ", 1)[1].strip() for l in p.group(1).splitlines() if l.strip().startswith("- ")] if p else []

missing = []
for event in ("push", "pull_request"):
    pats = filters.get(event)
    if pats is None:
        missing.append(f"{event}: no paths filter block found")
        continue
    for path in inputs:
        # A file input matches itself; a directory input matches `dir/**`.
        if not any(fnmatch.fnmatchcase(path, pat) or fnmatch.fnmatchcase(f"{path}/x", pat) for pat in pats):
            missing.append(f"{event}: {path}")
if missing:
    print("error: Release workflow does not trigger for release inputs:", *missing, sep="\n  ", file=sys.stderr)
    sys.exit(1)
print(f"ok: {len(inputs)} release inputs covered by push and pull_request filters")
