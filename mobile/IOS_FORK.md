# Install your fork on an iPhone

Build this checkout with your Apple team's signing identity.
Your fork can coexist with the older Buzz app under its own bundle ID.
Later builds update that fork when you keep the bundle ID and signing team unchanged.
An upstream TestFlight installation does not receive changes merged only into this fork.

## Before the first install

Use a Mac with full Xcode, CocoaPods, and access to an Apple Developer Program team.
These instructions retain Buzz's existing capabilities; a free Personal Team may not support them.
Open Xcode once to install its components and accept its license.
Sign in under **Xcode → Settings → Apple Accounts**.

Clone `voltagecloud/buzz`, or update your clean checkout to the reviewed revision on `main`.
From the repository root, activate the pinned tools:

```bash
. ./bin/activate-hermit
just mobile-ios-setup io.voltage.buzz.mobile YOURTEAMID "Buzz Voltage"
```

Replace `YOURTEAMID` with your ten-character Apple Team ID.
Choose a bundle ID that your team owns; `io.voltage.buzz.mobile` is an example.
The command writes `mobile/ios/Flutter/AppOverrides.xcconfig`, which Git ignores.
It preserves unrelated overrides, including your push gateway and capability environments.
It does not create Apple registrations, certificates, or provisioning profiles.

Ask your Apple team administrator to provision these identifiers:

| Identifier | Required configuration |
| --- | --- |
| Your bundle ID | App Groups, Push Notifications, App Attest, Declared Age Range, and Communication Notifications |
| `<bundle-id>.NotificationService` | App Groups |
| `group.<bundle-id>` | Associate this App Group with both targets |

Use the entitlements in `ios/Runner/Runner.entitlements` and `ios/NotificationService/NotificationService.entitlements` as the source of truth.
After enabling capabilities, regenerate any manually managed profiles.
The setup command selects automatic signing; Xcode still needs access to the matching team and capabilities.

Connect and unlock your iPhone, then trust the Mac when prompted.
Enable **Settings → Privacy & Security → Developer Mode** on the phone when Xcode requests it.

## Install or update

Find the physical iPhone's device ID:

```bash
flutter devices
```

Run the device command with that ID:

```bash
just mobile-ios-device YOUR_IPHONE_DEVICE_ID
```

The command activates Hermit, resolves Flutter packages, installs native dependencies through Flutter, and runs the debug build.
It uses your saved signing identity, including when the checkout is a Git worktree.
If signing needs attention, open `mobile/ios/Runner.xcworkspace` in Xcode.
Select your phone and inspect **Signing & Capabilities** for both **Runner** and **NotificationService**.

For later updates, update the same checkout to the reviewed revision and repeat the device command.
No checkout reset, app uninstall, or identity change is needed.
Sign into the fork with your existing Buzz identity and the same community.
Keep the older app until you have confirmed that identity and community access in the fork.

## Push and agent launch

Push is optional for running Buzz and starting agents while the app is open.
Without a configured gateway, the build leaves push unavailable.
Do not reuse an upstream gateway for a different bundle ID.
Push setup requires matching gateway application identifiers and signing environments; see [the mobile README](README.md#ios-push-capability).

The server must also support mobile launches, and each agent needs its one-time desktop registration.
See [the launch rollout](../docs/mobile-remote-agent-launch.md#rollout).
Installing this app does not deploy the manager or grant cluster permissions.

## TestFlight distribution

For installation without a development Mac, an Apple team member must sign and upload a build of this fork.
Use a stable bundle ID and grant testers access in your own App Store Connect application.
The inherited `mobile-release.sh` and release-candidate workflow target `block/buzz`; they do not publish this fork.
This setup command prepares local device builds, not an automated TestFlight release pipeline.

## Troubleshooting

| Symptom | Next action |
| --- | --- |
| No phone in `flutter devices` | Unlock it, trust the Mac, and check Xcode's Devices and Simulators window. |
| Unsupported entitlement or profile mismatch | Have the team administrator enable the listed capability and refresh the profile. |
| Bundle ID belongs to another team | Choose your team's ID and rerun setup; this installs a separate app. |
| Installation cannot replace the old app | Confirm both its bundle ID and signing identity; use a separate fork ID when they differ. |
| `unauthorized_owner` on agent startup | An administrator must provision your Buzz public key in the staging developer list. |
| One-time desktop setup message | Start that agent once from desktop after the manager upgrade, then retry from mobile. |

[Apple's device instructions](https://developer.apple.com/documentation/xcode/running-your-app-on-simulated-or-physical-devices)
describe signing and device setup.
[Flutter's iOS setup](https://docs.flutter.dev/platform-integration/ios/setup)
describes the host tools.
