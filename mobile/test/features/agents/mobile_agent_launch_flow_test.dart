import 'dart:async';
import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';

import 'launch_fixture.dart';

void main() {
  test('resume skips a message while its publication is in flight', () async {
    final fixture = LaunchFixture();
    addTearDown(fixture.dispose);
    fixture.relay.messageAck = Completer<void>();
    fixture.relay.beforePublish = (event) {
      if (event.kind == 9) fixture.queue.resume();
    };
    final sending = fixture.sender()(
      channelId: channelId,
      content: '@Bifrost work',
      mentionPubkeys: [agent.public],
    );
    await fixture.settle(() => fixture.relay.published.isNotEmpty);
    fixture.queue.resume();
    await Future<void>.delayed(Duration.zero);
    expect(fixture.relay.requests, isEmpty);
    expect(fixture.entries.single.error, isNull);
    fixture.relay.messageAck!.complete();
    await sending;
    await fixture.settle(() => fixture.entries.isEmpty);
    expect(fixture.relay.requests, hasLength(1));
  });

  test(
    'oversized launch messages fail before publication instead of creating an unretryable intent',
    () async {
      final fixture = LaunchFixture();
      addTearDown(fixture.dispose);
      await expectLater(
        fixture.sender()(
          channelId: channelId,
          content: 'x' * 60001,
          mentionPubkeys: [agent.public],
        ),
        throwsArgumentError,
      );
      expect(fixture.relay.published, isEmpty);
      expect(fixture.entries, isEmpty);
    },
  );

  test('pending launches wait for relay reconnection', () async {
    final fixture = LaunchFixture();
    addTearDown(fixture.dispose);
    fixture.connected = false;
    await fixture.sender()(
      channelId: channelId,
      content: '@Bifrost work',
      mentionPubkeys: [agent.public],
    );
    expect(fixture.relay.requests, isEmpty);
    fixture.connected = true;
    fixture.queue.resume();
    await fixture.settle(() => fixture.entries.isEmpty);
    expect(fixture.relay.requests, hasLength(1));
  });

  test(
    'a mobile mention persists its intent before publishing and launches its exact signed root',
    () async {
      final fixture = LaunchFixture();
      addTearDown(fixture.dispose);
      fixture.relay.messageAck = Completer<void>();
      fixture.relay.beforePublish = (event) {
        if (event.kind == 9) {
          final saved = jsonDecode(fixture.stored!) as List;
          expect(saved.single['message']['id'], event.id);
          expect(fixture.relay.requests, isEmpty);
        }
      };
      final send = fixture.sender()(
        channelId: channelId,
        content: '@Bifrost investigate',
        mentionPubkeys: [agent.public],
      );
      await fixture.settle(() => fixture.relay.published.isNotEmpty);
      expect(
        fixture.relay.requests,
        isEmpty,
        reason: 'A start must wait for the message OK',
      );
      fixture.relay.messageAck!.complete();
      await send;
      await fixture.settle(
        () => fixture.relay.requests.isNotEmpty && fixture.entries.isEmpty,
      );
      final request = fixture.relay.requests.single;
      expect(request['op'], 'start');
      expect(
        request['payload']['trigger'],
        fixture.relay.published.first.toJson(),
      );
      expect(request['payload']['agent_pubkey'], agent.public);
      expect(request.toString(), isNot(contains('private_key_nsec')));
      expect(fixture.relay.published.last.pubkey, owner.public);
      expect(fixture.relay.unsubscribed, 1);
    },
  );

  test(
    'retry escapes a cached manager refusal without duplicating the sent message',
    () async {
      final fixture = LaunchFixture();
      addTearDown(fixture.dispose);
      fixture.relay.refusal = 'launch_not_registered';
      fixture.relay.cacheFailures = true;
      final removed = <String>[];
      await fixture.sender(removeLocal: (_, id) => removed.add(id))(
        channelId: channelId,
        content: '@Bifrost retry',
        mentionPubkeys: [agent.public],
        parentEventId: 'b' * 64,
        rootEventId: 'a' * 64,
      );
      await fixture.settle(() => fixture.entries.single.error != null);
      final entry = fixture.entries.single;
      expect(entry.threadRoot, 'a' * 64);
      expect(
        removed,
        isEmpty,
        reason: 'Agent failure cannot roll back an accepted message',
      );
      final firstId = fixture.relay.requests.single['request_id'];
      fixture.relay.refusal = null;
      await fixture.queue.retry(entry.id);
      await fixture.settle(() => fixture.entries.isEmpty);
      expect(
        fixture.relay.published.where((event) => event.kind == 9),
        hasLength(1),
      );
      expect(fixture.relay.requests.last['request_id'], isNot(firstId));
      expect(
        fixture.relay.requests.last['payload'],
        fixture.relay.requests.first['payload'],
      );
    },
  );

  test(
    'an interrupted explicit retry restores its persisted operation ID',
    () async {
      final fixture = LaunchFixture();
      addTearDown(fixture.dispose);
      fixture.relay.refusal = 'launch_not_registered';
      fixture.relay.cacheFailures = true;
      await fixture.sender()(
        channelId: channelId,
        content: '@Bifrost resume',
        mentionPubkeys: [agent.public],
      );
      await fixture.settle(() => fixture.entries.single.error != null);
      fixture.relay.answer = false;
      await fixture.queue.retry(fixture.entries.single.id);
      await fixture.settle(() => fixture.relay.requests.length == 2);
      final retryId = fixture.relay.requests.last['request_id'];
      final journal = fixture.stored;
      fixture.dispose();

      final restored = LaunchFixture()..stored = journal;
      addTearDown(restored.dispose);
      await restored.queue.ready;
      restored.queue.resume();
      await restored.settle(() => restored.entries.isEmpty);
      expect(restored.relay.requests.single['request_id'], retryId);
      expect(
        restored.relay.published.where((event) => event.kind == 9),
        isEmpty,
      );
    },
  );

  test(
    'a restart reconciles a pre-publication intent against the relay before starting',
    () async {
      final fixture = LaunchFixture();
      addTearDown(fixture.dispose);
      final event = signed('33' * 32, 9, '@Bifrost work', [
        ['h', channelId],
        ['p', agent.public],
      ]);
      await fixture.queue.prepare(event);
      fixture.relay.published.add(
        event,
      ); // Relay accepted before the old process stopped.
      final restored = fixture.newQueue();
      await restored.ready;
      restored.resume();
      await fixture.settle(() => fixture.entries.isEmpty);
      expect(
        fixture.relay.requests.single['payload']['trigger']['id'],
        event.id,
      );
      expect(
        fixture.relay.published.where((event) => event.kind == 9),
        hasLength(1),
      );
    },
  );

  test('an unaccepted message cannot start an agent after restart', () async {
    final fixture = LaunchFixture();
    addTearDown(fixture.dispose);
    final event = signed('33' * 32, 9, '@Bifrost work', [
      ['h', channelId],
      ['p', agent.public],
    ]);
    await fixture.queue.prepare(event);
    final restored = fixture.newQueue();
    await restored.ready;
    restored.resume();
    await fixture.settle(() => fixture.entries.single.error != null);
    expect(fixture.relay.requests, isEmpty);
    expect(fixture.entries.single.error, contains('not found'));
  });

  test(
    'a failed durable intent prevents message publication and preserves the draft path',
    () async {
      final fixture = LaunchFixture();
      addTearDown(fixture.dispose);
      fixture.denySave = true;
      final removed = <String>[];
      await expectLater(
        fixture.sender(removeLocal: (_, id) => removed.add(id))(
          channelId: channelId,
          content: '@Bifrost work',
          mentionPubkeys: [agent.public],
        ),
        throwsStateError,
      );
      expect(fixture.relay.published, isEmpty);
      expect(removed, hasLength(1));
    },
  );

  test(
    'another owner, local agent, and display-only reference never request startup',
    () async {
      for (final record in [
        signed('44' * 32, 30177, '{}', [
          ['d', agent.public],
          ['runtime', 'voltage.remote-workspace/sandbox'],
        ]),
        signed('33' * 32, 30177, '{}', [
          ['d', agent.public],
          ['runtime', 'local/sandbox'],
        ]),
      ]) {
        final fixture = LaunchFixture();
        addTearDown(fixture.dispose);
        fixture.relay.listings = [record];
        await fixture.sender()(
          channelId: channelId,
          content: '@Bifrost',
          mentionPubkeys: [agent.public],
        );
        expect(fixture.relay.requests, isEmpty);
      }
      final fixture = LaunchFixture();
      addTearDown(fixture.dispose);
      await fixture.sender()(
        channelId: channelId,
        content: 'referencing Bifrost',
        mentionPubkeys: [],
        mediaTags: [
          ['mention', agent.public],
        ],
      );
      expect(fixture.relay.requests, isEmpty);
    },
  );

  test('switching communities while preparing cancels publication', () async {
    final fixture = LaunchFixture();
    addTearDown(fixture.dispose);
    fixture.onSave = () => fixture.current = false;
    final send = fixture.sender()(
      channelId: channelId,
      content: '@Bifrost',
      mentionPubkeys: [agent.public],
    );
    await expectLater(send, throwsStateError);
    expect(fixture.relay.published, isEmpty);
    expect(fixture.relay.requests, isEmpty);
  });

  test(
    'a missing registration has a useful failure and remains retryable',
    () async {
      final fixture = LaunchFixture();
      addTearDown(fixture.dispose);
      fixture.relay.refusal = 'launch_not_registered';
      await fixture.sender()(
        channelId: channelId,
        content: '@Bifrost',
        mentionPubkeys: [agent.public],
      );
      await fixture.settle(() => fixture.entries.single.error != null);
      expect(fixture.entries.single.error, contains('once from desktop'));
      expect(jsonDecode(fixture.stored!) as List, hasLength(1));
      expect(fixture.entries.single.error, isNot(contains('nsec')));
    },
  );
}
