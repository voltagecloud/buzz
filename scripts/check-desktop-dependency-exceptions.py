#!/usr/bin/env python3
"""Keep the desktop advisory exceptions bound to the code paths they excuse.

- ringbuf: an unused dependency of neteq 0.8.3.
- zip < 9: reachable only through the optional mesh-llm feature, whose
  readers never call the affected extract APIs.
"""

import json
from pathlib import Path
import re
import subprocess


def validate(metadata):
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = metadata["resolve"]["nodes"]
    affected = [
        package
        for package in packages.values()
        if package["name"] == "ringbuf"
    ]
    if len(affected) != 1 or affected[0]["version"] != "0.3.3":
        raise ValueError("reassess the ringbuf exception: expected exactly version 0.3.3")
    consumers = [
        packages[node["id"]]
        for node in nodes
        if affected[0]["id"] in node["dependencies"]
    ]
    if len(consumers) != 1:
        raise ValueError("the ringbuf exception requires neteq to be its only consumer")
    neteq = consumers[0]
    if (neteq["name"], neteq["version"]) != ("neteq", "0.8.3"):
        raise ValueError("reassess the ringbuf exception for the new consumer")
    if not (neteq["source"] or "").startswith("registry+"):
        raise ValueError("reassess the ringbuf exception for the patched neteq source")
    dependency = next(dep for dep in neteq["dependencies"] if dep["name"] == "ringbuf")
    if dependency["rename"] is not None:
        raise ValueError("neteq must not alias the unused ringbuf dependency")
    if any("custom-build" in target["kind"] for target in neteq["targets"]):
        raise ValueError("reassess neteq source generation before allowing ringbuf")
    sources = list(Path(neteq["manifest_path"]).parent.joinpath("src").rglob("*.rs"))
    if not sources or any(re.search(r"\bringbuf\b", path.read_text()) for path in sources):
        raise ValueError("neteq must not reference ringbuf in its Rust source")


# The extract APIs that RUSTSEC-2026-0367/0368/0369 affect.
ZIP_EXTRACT = re.compile(r"\bZipStreamReader\b|\.extract(_unwrapped_root_dir)?\s*\(")
MESH_LLM_SOURCE = "git+https://github.com/Mesh-LLM/mesh-llm.git"


def vulnerable_zips(metadata):
    return [
        package
        for package in metadata["packages"]
        if package["name"] == "zip" and int(package["version"].split(".")[0]) < 9
    ]


def validate_zip(default_metadata, all_features_metadata):
    if vulnerable_zips(default_metadata):
        raise ValueError("the default desktop graph must not contain zip below 9")
    packages = {package["id"]: package for package in all_features_metadata["packages"]}
    nodes = all_features_metadata["resolve"]["nodes"]
    for affected in vulnerable_zips(all_features_metadata):
        consumers = [
            packages[node["id"]]
            for node in nodes
            if affected["id"] in node["dependencies"]
        ]
        if not consumers:
            raise ValueError("reassess the zip exception: zip below 9 has no consumer")
        for consumer in consumers:
            if not (consumer["source"] or "").startswith(MESH_LLM_SOURCE):
                raise ValueError(
                    f"zip {affected['version']} reached by {consumer['name']}; "
                    "the exception covers only mesh-llm"
                )
            sources = Path(consumer["manifest_path"]).parent.joinpath("src").rglob("*.rs")
            for path in sources:
                if ZIP_EXTRACT.search(path.read_text()):
                    raise ValueError(f"{path} calls a zip extract API the advisories affect")


def cargo_metadata(*flags):
    return json.loads(
        subprocess.check_output(
            [
                "cargo", "metadata", "--format-version", "1", "--locked", *flags,
                "--manifest-path", "desktop/src-tauri/Cargo.toml",
            ],
            text=True,
        )
    )


if __name__ == "__main__":
    all_features = cargo_metadata("--all-features")
    validate(all_features)
    validate_zip(cargo_metadata(), all_features)
    print("Desktop ringbuf exception remains confined to neteq 0.8.3's unused dependency")
    print("Desktop zip < 9 remains confined to mesh-llm, which never calls the affected extract APIs")
