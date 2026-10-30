"""Exercise the production native branding gate and override preparation."""
import base64
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
GATE = ROOT / 'mobile/scripts/require-app-brand.sh'


def encoded(value):
    return base64.b64encode(value.encode()).decode()


class BrandingTests(unittest.TestCase):
    def gate(self, **changes):
        env = {key: value for key, value in os.environ.items()
               if key not in ('DART_DEFINES', 'BUZZ_APP_VARIANT')}
        env.update(changes)
        return subprocess.run(
            ['sh', '-c', '. "$1"; printf "%s" "${DART_DEFINES:-}"', 'brand', str(GATE)],
            env=env, text=True, capture_output=True, timeout=5,
        )

    def commons(self, **changes):
        env = dict(BUZZ_APP_VARIANT='voltage-commons',
                   PRODUCT_BUNDLE_IDENTIFIER='cloud.voltage.workspace',
                   APP_DISPLAY_NAME='Voltage Commons', BUZZ_APP_ICON_NAME='CommonsAppIcon',
                   BUZZ_URL_SCHEME='voltage-workspace', BUZZ_LAUNCH_STORYBOARD='CommonsLaunchScreen')
        env.update(changes)
        return self.gate(**env)

    def test_upstream_build_keeps_existing_defines(self):
        original = encoded('OTHER=kept')
        result = self.gate(DART_DEFINES=original)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, original)

    def test_native_build_forwards_brand_without_losing_other_defines(self):
        original = encoded('OTHER=kept')
        result = self.commons(DART_DEFINES=original)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.split(','), [original, encoded('BUZZ_APP_VARIANT=voltage-commons')])

    def test_conflicts_duplicates_unknown_variants_and_block_identity_fail(self):
        for changes in [
            {'DART_DEFINES': encoded('BUZZ_APP_VARIANT=other')},
            {'DART_DEFINES': ','.join([encoded('BUZZ_APP_VARIANT=voltage-commons')] * 2)},
            {'PRODUCT_BUNDLE_IDENTIFIER': 'xyz.block.buzz.mobile'},
            {'BUZZ_APP_ICON_NAME': 'AppIcon'},
            {'BUZZ_URL_SCHEME': 'buzz'},
            {'BUZZ_LAUNCH_STORYBOARD': 'LaunchScreen'},
            {'BUZZ_APP_VARIANT': 'unknown'},
        ]:
            with self.subTest(changes=changes):
                self.assertNotEqual(self.commons(**changes).returncode, 0)
        self.assertNotEqual(self.gate(DART_DEFINES=encoded('BUZZ_APP_VARIANT=voltage-commons')).returncode, 0)

    def test_xcode_executes_gate_before_flutter_build(self):
        project = (ROOT / 'mobile/ios/Runner.xcodeproj/project.pbxproj').read_text()
        self.assertLess(project.index('require-app-brand.sh'), project.index('xcode_backend.sh\\\" build'))

    def test_preparation_never_overwrites_existing_signing_settings(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for relative in ['scripts', 'voltage/mobile', 'mobile/ios/Flutter']:
                (root / relative).mkdir(parents=True, exist_ok=True)
            for relative in ['scripts/prepare-commons-ios.sh', 'voltage/mobile/Commons.xcconfig']:
                shutil.copyfile(ROOT / relative, root / relative)
            env = {**os.environ, 'VOLTAGE_APPLE_TEAM_ID': 'ABCDEFGHIJ'}
            command = ['bash', str(root / 'scripts/prepare-commons-ios.sh')]
            first = subprocess.run(command, env=env, capture_output=True, timeout=5)
            self.assertEqual(first.returncode, 0, first.stderr)
            config = root / 'mobile/ios/Flutter/AppOverrides.xcconfig'
            saved = config.read_bytes()
            self.assertIn(b'BUZZ_DEVELOPMENT_TEAM = ABCDEFGHIJ', saved)
            second = subprocess.run(command, env=env, capture_output=True, timeout=5)
            self.assertNotEqual(second.returncode, 0)
            self.assertEqual(config.read_bytes(), saved)


if __name__ == '__main__':
    unittest.main()
