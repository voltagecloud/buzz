import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

/// The distribution selected at build time. Protocol names remain unchanged.
const isVoltageCommons =
    String.fromEnvironment('BUZZ_APP_VARIANT') == 'voltage-commons';

/// The user-facing application name, independent of its permanent bundle ID.
const appDisplayName = isVoltageCommons ? 'Voltage Commons' : 'Buzz';

/// Attribution shown by the independent Voltage distribution.
const upstreamAttribution =
    'Built on Buzz, an open-source project by Block, Inc. '
    'This modified client is maintained by Voltage and is not an official '
    'Block release.\n\nSource: https://github.com/block/buzz\n'
    'Voltage modifications: https://github.com/voltagecloud/buzz';

/// Includes the upstream license in Flutter's in-app license viewer.
void registerDistributionLicenses() {
  if (!isVoltageCommons) return;
  LicenseRegistry.addLicense(() async* {
    final license = await rootBundle.loadString(
      'assets/licenses/buzz-LICENSE.txt',
    );
    yield LicenseEntryWithLineBreaks(['Buzz — Block, Inc.'], license);
  });
}
