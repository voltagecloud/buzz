"""Pins a release's AppImages in nix/release.json, from which the flake
builds buzz-gpui-voltage-bin: the release's AppImage for each Linux system,
by URL and by the SRI hash that fetchurl checks.

Usage: python3 scripts/pin-nix-release.py <version> <owner/repo> <dir>
where <dir> holds Buzz-<version>-linux-{x86_64,aarch64}.AppImage.
"""

import base64
import hashlib
import json
import pathlib
import sys
import urllib.parse

# AppImage architecture -> Nix system.
SYSTEMS = {"x86_64": "x86_64-linux", "aarch64": "aarch64-linux"}
PIN = pathlib.Path(__file__).resolve().parent.parent / "nix" / "release.json"


def pin(version, repository, directory):
    assets = {}
    for arch, system in SYSTEMS.items():
        name = f"Buzz-{version}-linux-{arch}.AppImage"
        with open(pathlib.Path(directory) / name, "rb") as image:
            digest = hashlib.file_digest(image, "sha256").digest()
        tag = urllib.parse.quote(f"v{version}", safe="")
        assets[system] = {
            "url": f"https://github.com/{repository}/releases/download/{tag}/"
            + urllib.parse.quote(name, safe=""),
            "hash": "sha256-" + base64.b64encode(digest).decode(),
        }
    return {"version": version, "assets": assets}


if __name__ == "__main__":
    version, repository, directory = sys.argv[1:]
    PIN.write_text(json.dumps(pin(version, repository, directory), indent=2) + "\n")
    print(PIN.read_text(), end="")
