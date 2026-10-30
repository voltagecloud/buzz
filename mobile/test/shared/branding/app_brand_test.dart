import 'package:buzz/features/pairing/pairing_page/onboarding_wordmark.dart';
import 'package:buzz/shared/branding/app_brand.dart';
import 'package:buzz/shared/branding/commons_mark.dart';
import 'package:buzz/shared/deeplink/deep_link.dart';
import 'package:buzz/shared/widgets/flapping_bee.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  testWidgets(
    'welcome uses the selected distribution and one accessible label',
    (tester) async {
      final semantics = tester.ensureSemantics();
      await tester.pumpWidget(
        MaterialApp(
          home: MediaQuery(
            data: const MediaQueryData(
              disableAnimations: true,
              textScaler: TextScaler.linear(2),
            ),
            child: const Center(
              child: SizedBox(
                width: 280,
                height: 140,
                child: OnboardingWordmark(),
              ),
            ),
          ),
        ),
      );
      await tester.pumpAndSettle();
      expect(find.bySemanticsLabel(appDisplayName), findsOneWidget);
      expect(
        find.byType(CommonsMark),
        isVoltageCommons ? findsOneWidget : findsNothing,
      );
      expect(
        find.text('Voltage Commons'),
        isVoltageCommons ? findsOneWidget : findsNothing,
      );
      expect(tester.takeException(), isNull);
      semantics.dispose();
    },
  );

  testWidgets('shared loading mark uses the distribution artwork', (
    tester,
  ) async {
    await tester.pumpWidget(
      const MaterialApp(
        home: Center(
          child: FlappingBee(width: 60, color: Colors.teal, flapAmount: 0),
        ),
      ),
    );
    await tester.pumpAndSettle();
    expect(
      find.byType(CommonsMark),
      isVoltageCommons ? findsOneWidget : findsNothing,
    );
    expect(tester.takeException(), isNull);
  });

  test('independent OS handoffs are accepted only in Commons builds', () {
    const channel = 'fe3d5502-7481-4220-9573-82fa58f70017';
    final standard = Uri.parse('buzz://channel/$channel');
    final commons = Uri.parse('voltage-workspace://channel/$channel');
    expect(parseChannelDeepLink(standard), isNotNull);
    expect(
      parseChannelDeepLink(commons),
      isVoltageCommons ? isNotNull : isNull,
    );
    final message = Uri.parse(
      'voltage-workspace://message?channel=$channel&id=${'a' * 64}',
    );
    expect(
      parseMessageDeepLink(message),
      isVoltageCommons ? isNotNull : isNull,
    );
    final invite = Uri.parse(
      'voltage-workspace://join?relay=wss%3A%2F%2Frelay.example&code=hello',
    );
    expect(parseInviteDeepLink(invite), isVoltageCommons ? isNotNull : isNull);
  });

  test('distribution packages the complete upstream license', () async {
    final license = await rootBundle.loadString(
      'assets/licenses/buzz-LICENSE.txt',
    );
    expect(license, contains('Copyright 2026 Block, Inc.'));
    expect(license, contains('6. Trademarks.'));
  });
}
