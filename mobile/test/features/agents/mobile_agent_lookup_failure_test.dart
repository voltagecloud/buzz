import 'package:buzz/shared/relay/relay.dart';
import 'package:flutter_test/flutter_test.dart';

import 'launch_fixture.dart';

void main() {
  test(
    'lookup failures leave ordinary mentions sendable and explain skipped startup',
    () async {
      final fixture = LaunchFixture();
      addTearDown(fixture.dispose);
      fixture.relay.listingError = StateError('history unavailable');
      await fixture.sender()(
        channelId: channelId,
        content: '@Alice hello',
        mentionPubkeys: [agent.public],
      );
      expect(fixture.relay.published.map((event) => event.kind), [9]);
      expect(fixture.relay.requests, isEmpty);
      expect(fixture.entries, isEmpty);
      expect(
        fixture.notices.single.message.id,
        fixture.relay.published.single.id,
      );
      expect(fixture.notices.single.text, contains('startup was skipped'));
    },
  );

  test(
    'malformed and forged listings cannot block publication or authorize startup',
    () async {
      for (final forged in [false, true]) {
        final fixture = LaunchFixture();
        addTearDown(fixture.dispose);
        var record = signed('33' * 32, 30177, 'unreadable', [
          ['d', agent.public],
          ['runtime', 'voltage.remote-workspace/sandbox'],
        ]);
        if (forged) {
          record = NostrEvent.fromJson(record.toJson()..['sig'] = '0' * 128);
        }
        fixture.relay.listings = [record];
        await fixture.sender()(
          channelId: channelId,
          content: '@Alice hello',
          mentionPubkeys: [agent.public],
        );
        expect(fixture.relay.published.map((event) => event.kind), [9]);
        expect(fixture.relay.requests, isEmpty);
        expect(fixture.entries, isEmpty);
      }
    },
  );

  test(
    'a broken launch journal does not block a confirmed ordinary mention',
    () async {
      final fixture = LaunchFixture();
      addTearDown(fixture.dispose);
      fixture.stored = 'invalid journal';
      await expectLater(fixture.queue.ready, throwsFormatException);
      fixture.relay.listings = [];
      await fixture.sender()(
        channelId: channelId,
        content: '@Alice hello',
        mentionPubkeys: [agent.public],
      );
      expect(fixture.relay.published.map((event) => event.kind), [9]);
      expect(fixture.relay.requests, isEmpty);
    },
  );
}
