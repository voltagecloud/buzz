# Voltage Commons for iPhone

Voltage Commons is the working name of Voltage's modified Buzz client.
The stable proposed bundle identifier is `cloud.voltage.workspace`.
The name can change without replacing that identifier.

Ordinary builds retain the upstream Buzz identity and artwork.
Commons builds use a separate icon, welcome screen, permission prompts, and About entry.
The About entry credits Block and links to both source repositories.
The license viewer includes the upstream Apache 2.0 license.
The iOS build registers `voltage-workspace://` instead of claiming the official `buzz://` OS scheme.
Pasted and scanned Buzz links remain supported, and shared message links retain the canonical Buzz format.
Pairing codes and agent identities retain their existing formats.

## Configure an iOS build

Activate the repository's Hermit environment and install mobile dependencies.
Then run:

```sh
VOLTAGE_APPLE_TEAM_ID=YOURTEAMID ./scripts/prepare-commons-ios.sh
```

The script creates the gitignored `mobile/ios/Flutter/AppOverrides.xcconfig`.
It refuses to replace an existing file.
Without a team ID, the configuration supports unsigned builds only.
It never inherits Block's signing team.

On a macOS builder, check the complete unsigned application:

```sh
cd mobile
flutter build ios --release --no-codesign --no-pub
```

The Xcode build phase forwards the distribution to Flutter.
It rejects conflicting Dart defines and native branding.

To check the Flutter presentation from Linux:

```sh
cd mobile
flutter test --no-pub --dart-define=BUZZ_APP_VARIANT=voltage-commons \
  test/shared/branding/app_brand_test.dart
```

## Device distribution prerequisites

No signed build or TestFlight upload is configured by this change.
Voltage must supply an active Apple Developer membership and distribution credentials.
Register the app identifier, its `.NotificationService` extension, and `group.cloud.voltage.workspace` under that account.
Configure the required capabilities and matching provisioning profiles for both targets.
Use Voltage's own support and privacy information in App Store Connect.

A GitHub macOS runner can build and upload from this repository.
The developer's computer can remain on NixOS.
Keep signing credentials in GitHub secrets; never commit them.

For an initial test without push, omit `BUZZ_PUSH_GATEWAY_URL`.
Push requires a gateway configured for the new app identifier and signing team.
See [mobile setup](../../mobile/README.md#ios-push-capability).

Use TestFlight for genuine testing and evaluation.
For ongoing employee distribution, evaluate Apple's private Custom App distribution.
Both distribution and naming remain subject to Apple's review and applicable rights.
No permission to reuse Buzz's name or icon is assumed.

## Upstream coordination

Before wider distribution, the human maintainer can send this note:

> We use Buzz for Voltage's workspace and are testing a modified client for our team.
> We plan to publish it under a separate name, icon, and Apple account.
> We will preserve upstream notices, credit Block prominently, and contribute general fixes upstream.
> Do you have additional fork-branding or distribution guidance we should follow?

This draft has not been sent.
