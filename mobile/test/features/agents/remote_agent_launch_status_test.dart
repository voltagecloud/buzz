import 'package:buzz/shared/agents/agent_launch.dart';
import 'package:buzz/shared/agents/remote_agent_launch_provider.dart';
import 'package:buzz/shared/agents/remote_agent_launch_status.dart';
import 'package:buzz/shared/relay/relay.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:hooks_riverpod/hooks_riverpod.dart';

import 'launch_fixture.dart';

class _Launches extends RemoteAgentLaunches {
  final LaunchFixture fixture;
  _Launches(this.fixture);
  @override
  AsyncValue<List<AgentLaunch>> build() {
    queue = fixture.queue;
    fixture.onEntries = (entries) => state = AsyncData(entries);
    ref.onDispose(() => fixture.onEntries = null);
    return AsyncData(fixture.entries);
  }
}

class _Notice extends RemoteAgentLaunchNotice {
  final AgentLaunchNotice notice;
  _Notice(this.notice);
  @override
  AgentLaunchNotice? build() => notice;
}

void main() {
  test(
    'queue storage separates full relay endpoints and preserves scheme aliases',
    () {
      String key(String endpoint) => remoteAgentLaunchStorageKey(
        RelayConfig(baseUrl: endpoint, nsec: owner.nsec),
        owner.public,
      );
      expect(
        key('https://relay.example/a?network=one'),
        key('wss://relay.example/a?network=one'),
      );
      expect(
        key('https://relay.example/a?network=one'),
        isNot(key('https://relay.example/b?network=one')),
      );
      expect(
        key('https://relay.example/a?network=one'),
        isNot(key('https://relay.example/a?network=two')),
      );
    },
  );

  testWidgets('skipped startup has a dismissible notice in its own thread', (
    tester,
  ) async {
    final fixture = LaunchFixture();
    addTearDown(fixture.dispose);
    final message = signed('33' * 32, 9, '@Alice', [
      ['h', channelId],
      ['p', agent.public],
    ]);
    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          remoteAgentLaunchesProvider.overrideWith(() => _Launches(fixture)),
          remoteAgentLaunchNoticeProvider.overrideWith(
            () => _Notice(
              AgentLaunchNotice(message, 'Agent startup was skipped.'),
            ),
          ),
        ],
        child: MaterialApp(
          home: Scaffold(
            body: RemoteAgentLaunchStatus(
              channelId: channelId,
              threadRoot: message.id,
            ),
          ),
        ),
      ),
    );
    expect(find.text('Agent startup was skipped.'), findsOneWidget);
    await tester.tap(find.byTooltip('Dismiss agent startup notice'));
    await tester.pumpAndSettle();
    expect(find.text('Agent startup was skipped.'), findsNothing);
    expect(fixture.relay.published, isEmpty);
    expect(tester.takeException(), isNull);
  });

  testWidgets(
    'a visible startup failure retries without resending the message',
    (tester) async {
      final fixture = LaunchFixture();
      addTearDown(fixture.dispose);
      fixture.relay.refusal = 'launch_not_registered';
      await tester.runAsync(() async {
        await fixture.sender()(
          channelId: channelId,
          content: '@Bifrost',
          mentionPubkeys: [agent.public],
        );
        await fixture.settle(() => fixture.entries.single.error != null);
      });
      await tester.pumpWidget(
        ProviderScope(
          overrides: [
            remoteAgentLaunchesProvider.overrideWith(() => _Launches(fixture)),
          ],
          child: const MaterialApp(
            home: Scaffold(body: RemoteAgentLaunchStatus(channelId: channelId)),
          ),
        ),
      );
      expect(find.textContaining('once from desktop'), findsOneWidget);
      expect(find.byTooltip('Dismiss startup for Bifrost'), findsOneWidget);
      fixture.relay.refusal = null;
      await tester.runAsync(() async {
        await tester.tap(find.text('Retry'));
        await fixture.settle(() => fixture.entries.isEmpty);
      });
      await tester.pumpAndSettle();
      expect(find.textContaining('once from desktop'), findsNothing);
      expect(fixture.relay.requests, hasLength(2));
      expect(
        fixture.relay.published.where((event) => event.kind == 9),
        hasLength(1),
      );
      expect(tester.takeException(), isNull);
    },
  );
}
