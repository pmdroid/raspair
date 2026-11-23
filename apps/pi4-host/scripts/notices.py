#!/usr/bin/env python3
import json
import re
import subprocess
import sys
from pathlib import Path

LICENSE_PREFIXES = ("license", "licence", "copying", "notice", "ofl", "unlicense")
FORBIDDEN = re.compile(r"(?<![L])GPL")


def license_files(directory):
    found = []
    for path in sorted(directory.iterdir()):
        if path.is_file() and path.name.lower().startswith(LICENSE_PREFIXES):
            found.append(path)
    return found


def rust_packages(workspace, target, features):
    tree = subprocess.run(
        ["cargo", "tree", "--locked", "-e", "normal", "--target", target, "-p", "vinyl-bridge-pi",
         "--no-default-features", "--features", features, "--prefix", "none", "--format", "{p}"],
        cwd=workspace, check=True, capture_output=True, text=True).stdout
    wanted = set()
    for line in tree.splitlines():
        if "(proc-macro)" in line or not line.strip():
            continue
        name, version = line.split()[:2]
        wanted.add((name, version.lstrip("v")))
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target],
        cwd=workspace, check=True, capture_output=True, text=True).stdout)
    out = []
    for pkg in meta["packages"]:
        if (pkg["name"], pkg["version"]) in wanted:
            src = "workspace" if pkg["source"] is None else pkg["source"]
            out.append(("rust", pkg["name"], pkg["version"], pkg.get("license") or "", src,
                        Path(pkg["manifest_path"]).parent))
    missing = wanted - {(p[1], p[2]) for p in out}
    if missing:
        sys.exit(f"no metadata for {sorted(missing)}")
    return out


def web_packages(web):
    root = json.loads((web / "package.json").read_text())
    queue, seen, out = list(root.get("dependencies", {})), set(), []
    while queue:
        name = queue.pop()
        if name in seen:
            continue
        seen.add(name)
        directory = web / "node_modules" / name
        pkg = json.loads((directory / "package.json").read_text())
        out.append(("web", name, pkg["version"], pkg.get("license", ""), "npm", directory))
        queue.extend(pkg.get("dependencies", {}))
    return out


def main():
    workspace, web, notices_out, manifest_out = map(Path, sys.argv[1:5])
    target = sys.argv[5] if len(sys.argv) > 5 else "aarch64-unknown-linux-gnu"
    packages = rust_packages(workspace, target, "alsa") + web_packages(web)
    packages.sort(key=lambda p: (p[0], p[1], p[2]))
    errors = []
    notices = ["vinyl-bridge-pi third-party notices",
               "",
               "vinyl-bridge-pi is licensed under the MIT license. It links the",
               "packages below. libc, libgcc_s, libasound (LGPL-2.1+) and OpenSSL",
               "(Apache-2.0) are dynamically linked from Raspberry Pi OS and not",
               "shipped in this executable.",
               ""]
    manifest = []
    printed = {}
    for kind, name, version, lic, source, directory in packages:
        files = license_files(directory)
        if not lic:
            errors.append(f"{kind} {name} {version}: no license field")
        if FORBIDDEN.search(lic):
            errors.append(f"{kind} {name} {version}: license {lic} is not allowed")
        if not files and source != "workspace":
            errors.append(f"{kind} {name} {version}: no license file")
        manifest.append({"ecosystem": kind, "name": name, "version": version, "license": lic,
                         "source": source, "license_files": [f.name for f in files]})
        notices.append("=" * 72)
        notices.append(f"{name} {version} ({kind}) — {lic}")
        notices.append("=" * 72)
        for f in files:
            text = f.read_text(errors="replace").strip()
            if text in printed:
                notices.append(f"--- {f.name}: same text as {printed[text]}")
            else:
                printed[text] = f"{name} {version} {f.name}"
                notices.append(f"--- {f.name}")
                notices.append(text)
        notices.append("")
    if errors:
        sys.exit("\n".join(errors))
    notices_out.write_text("\n".join(notices) + "\n")
    manifest_out.write_text(json.dumps({"packages": manifest}, indent=2) + "\n")


main()
