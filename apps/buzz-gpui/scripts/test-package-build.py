"""Exercise the real package builder with a deterministic Cargo subprocess."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class PackageBuild(unittest.TestCase):
    def run_build(self, voltage, windows=False, fail=False):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        app = root / "apps" / "buzz-gpui"
        scripts = app / "scripts"
        scripts.mkdir(parents=True)
        shutil.copy2(Path(__file__).with_name("build-package.sh"), scripts)
        binaries = root / "bin"
        binaries.mkdir()
        cargo = binaries / "cargo"
        cargo.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
with open(os.environ["CALLS"], "a") as log:
    log.write(json.dumps(args) + "\\n")
provider = "buzz-backend-relay" in args
if provider and os.environ.get("FAIL_PROVIDER") == "1":
    sys.exit(9)
target = pathlib.Path(args[args.index("--target-dir")+1] if provider else os.environ["CARGO_TARGET_DIR"])
release = target / "release"
release.mkdir(parents=True, exist_ok=True)
name = "buzz-backend-relay" if provider else "buzz-gpui"
if os.environ.get("OS") == "Windows_NT":
    name += ".exe"
(release / name).write_text("fresh " + name)
''')
        cargo.chmod(0o755)
        env = dict(os.environ, PATH=str(binaries)+os.pathsep+os.environ["PATH"],
                   CALLS=str(root/"calls.jsonl"), CARGO_TARGET_DIR=str(root/"build"),
                   BUZZ_GPUI_VOLTAGE="1" if voltage else "0",
                   OS="Windows_NT" if windows else "Linux", FAIL_PROVIDER="1" if fail else "0")
        result = subprocess.run(["bash", str(scripts/"build-package.sh")],
                                env=env, capture_output=True, text=True, timeout=10)
        calls = [json.loads(line) for line in (root/"calls.jsonl").read_text().splitlines()]
        return root, result, calls

    def test_voltage_build_enables_remote_ui_and_stages_the_same_builds_provider(self):
        for windows in [False, True]:
            with self.subTest(windows=windows):
                root, result, calls = self.run_build(True, windows)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(len(calls), 2)
                self.assertIn(str(root/"Cargo.toml"), calls[0])
                self.assertIn("buzz-backend-relay", calls[0])
                self.assertIn("--features", calls[1])
                self.assertIn("voltage-sandbox", calls[1])
                name = "buzz-backend-relay" + (".exe" if windows else "")
                self.assertEqual((root/"build"/"release"/name).read_text(), "fresh "+name)

    def test_generic_packages_keep_their_existing_feature_set(self):
        root, result, calls = self.run_build(False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 1)
        self.assertNotIn("--features", calls[0])
        self.assertFalse((root/"build"/"release"/"buzz-backend-relay").exists())

    def test_provider_failure_stops_packaging_before_building_the_app(self):
        root, result, calls = self.run_build(True, fail=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(calls), 1)
        self.assertFalse((root/"build"/"release"/"buzz-gpui").exists())


if __name__ == "__main__":
    unittest.main()
