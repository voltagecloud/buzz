import 'dart:async';

import 'package:buzz/shared/agents/remote_agent_start_client.dart';
import 'package:buzz/shared/relay/relay.dart';
import 'package:flutter_test/flutter_test.dart';

import 'launch_fixture.dart';

void main() {
  NostrEvent trigger() => signed('33' * 32, 9, '@Bifrost investigate', [
    ['h', channelId],
    ['p', agent.public],
  ]);

  test(
    'a verified manager completion confirms startup even when relay OK is lost',
    () async {
      final fixture = LaunchFixture();
      addTearDown(fixture.dispose);
      fixture.relay.controlAck = Completer<void>();
      await fixture.client.start(agent.public, trigger());
      expect(fixture.relay.unsubscribed, 1);
      fixture.relay.controlAck!.completeError(
        StateError('late relay disconnect'),
      );
      await Future<void>.delayed(Duration.zero);
    },
  );

  test(
    'wrong signer, correlation and tampered replies cannot confirm a start',
    () async {
      final fixture = LaunchFixture();
      addTearDown(fixture.dispose);
      fixture.relay.answer = false;
      var complete = false;
      final started = fixture.client
          .start(agent.public, trigger())
          .then((_) => complete = true);
      await fixture.settle(() => fixture.relay.requests.isNotEmpty);
      final request = fixture.relay.requests.single;
      final sent = fixture.relay.published.single;
      fixture.relay.reply(
        sent,
        request,
        'done',
        response: {'ok': true},
        signingKey: '44' * 32,
      );
      await Future<void>.delayed(Duration.zero);
      expect(complete, false);
      fixture.relay.reply(
        sent,
        request,
        'done',
        response: {'ok': true},
        eventId: 'a' * 64,
      );
      await Future<void>.delayed(Duration.zero);
      expect(complete, false);
      fixture.relay.reply(
        sent,
        request,
        'done',
        response: {'ok': true},
        requestId: 'another-request',
      );
      await Future<void>.delayed(Duration.zero);
      expect(complete, false);
      final listener = fixture.relay.listener!;
      fixture.relay.listener = (event) {
        final tampered = event.toJson()..['sig'] = '0' * 128;
        listener(NostrEvent.fromJson(tampered));
      };
      fixture.relay.reply(sent, request, 'done', response: {'ok': true});
      await Future<void>.delayed(Duration.zero);
      expect(complete, false);
      fixture.relay.listener = listener;
      fixture.relay.reply(sent, request, 'done', response: {'ok': true});
      await started;
      expect(complete, true);
      expect(fixture.relay.unsubscribed, 1);
    },
  );

  test(
    'an unavailable manager produces a bounded actionable failure',
    () async {
      final relay = LaunchRelay()..answer = false;
      final client = RemoteAgentStartClient(
        session: relay,
        config: RelayConfig(baseUrl: 'https://relay.example', nsec: owner.nsec),
        manager: manager.public,
        isCurrent: () => true,
        acceptTimeout: const Duration(milliseconds: 30),
        finishTimeout: const Duration(seconds: 1),
      );
      addTearDown(client.dispose);
      await expectLater(
        client.start(agent.public, trigger()),
        throwsA(
          isA<AgentStartException>().having(
            (e) => e.message,
            'message',
            contains('did not answer'),
          ),
        ),
      );
      expect(relay.unsubscribed, 1);
    },
  );

  test(
    'switching accounts cancels in-flight requests and releases subscriptions',
    () async {
      final fixture = LaunchFixture();
      addTearDown(fixture.dispose);
      fixture.relay.answer = false;
      final result = fixture.client.start(agent.public, trigger());
      final failure = expectLater(result, throwsA(isA<AgentStartException>()));
      await fixture.settle(() => fixture.relay.requests.isNotEmpty);
      fixture.dispose();
      await failure;
      expect(fixture.relay.unsubscribed, 1);
    },
  );
}
