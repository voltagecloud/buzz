import 'dart:async';
import 'dart:convert';

import 'package:buzz/shared/agents/agent_launch.dart';
import 'package:buzz/shared/agents/mobile_agent_launch_queue.dart';
import 'package:buzz/shared/agents/remote_agent_start_client.dart';
import 'package:buzz/features/channels/channel_management_provider.dart';
import 'package:buzz/features/channels/send_message_provider.dart';
import 'package:buzz/shared/crypto/nip44.dart';
import 'package:buzz/shared/relay/relay.dart';
import 'package:nostr/nostr.dart' as nostr;

const channelId = '11111111-1111-4111-8111-111111111111';
final owner = nostr.Keys('33' * 32);
final agent = nostr.Keys('22' * 32);
final manager = nostr.Keys('11' * 32);

NostrEvent signed(
  String key,
  int kind,
  String content,
  List<List<String>> tags, {
  int? createdAt,
}) => NostrEvent.fromJson(
  nostr.Event.from(
    kind: kind,
    content: content,
    tags: tags,
    secretKey: key,
    createdAt: createdAt,
    verify: false,
  ).toMap(),
);

class LaunchRelay extends RelaySessionNotifier {
  final published = <NostrEvent>[];
  final requests = <Map<String, dynamic>>[];
  List<NostrEvent>? listings;
  Object? listingError;
  void Function(NostrEvent)? listener;
  Completer<void>? messageAck;
  Completer<void>? controlAck;
  void Function(NostrEvent)? beforePublish;
  String? refusal;
  bool cacheFailures = false;
  final cachedFailures = <String, Map<String, dynamic>>{};
  bool answer = true;
  int unsubscribed = 0;

  @override
  SessionState build() => const SessionState(status: SessionStatus.connected);

  @override
  Future<List<NostrEvent>> fetchHistory(
    NostrFilter filter, {
    Duration timeout = const Duration(seconds: 8),
  }) async {
    if (filter.kinds.contains(30177)) {
      if (listingError case final error?) throw error;
      return listings ??
          [
            signed('33' * 32, 30177, jsonEncode({'name': 'Bifrost'}), [
              ['d', agent.public],
              ['runtime', 'voltage.remote-workspace/sandbox'],
            ]),
          ];
    }
    return published
        .where((event) => filter.ids?.contains(event.id) == true)
        .toList();
  }

  @override
  Future<void Function()> subscribeWithStatus(
    NostrFilter filter,
    void Function(NostrEvent) onEvent, {
    void Function(String)? onClosed,
    required void Function(RelaySubscriptionStatus) onStatusChanged,
  }) async {
    listener = onEvent;
    onStatusChanged(RelaySubscriptionStatus.ready);
    return () {
      listener = null;
      unsubscribed++;
    };
  }

  @override
  Future<NostrEvent> publish(
    NostrEvent event, {
    Duration timeout = const Duration(seconds: 8),
  }) async {
    beforePublish?.call(event);
    published.add(event);
    if (event.kind == 9) {
      await messageAck?.future;
      return event;
    }
    if (event.kind == 24210) {
      final key = getConversationKey('11' * 32, owner.public);
      final request =
          jsonDecode(nip44Decrypt(key, event.content)) as Map<String, dynamic>;
      requests.add(request);
      if (answer) {
        reply(event, request, 'accepted');
        final response =
            cachedFailures[request['request_id']] ??
            (refusal == null
                ? {'ok': true, 'agent_id': 'fixture-sandbox'}
                : {'ok': false, 'code': refusal});
        if (cacheFailures && response['ok'] == false) {
          cachedFailures[request['request_id'] as String] = response;
        }
        reply(event, request, 'done', response: response);
      }
    }
    if (event.kind == 24210) await controlAck?.future;
    return event;
  }

  void reply(
    NostrEvent requestEvent,
    Map<String, dynamic> request,
    String status, {
    Map<String, dynamic>? response,
    String? eventId,
    String? requestId,
    String? signingKey,
  }) {
    final now = DateTime.now().millisecondsSinceEpoch ~/ 1000;
    final content = nip44Encrypt(
      getConversationKey('11' * 32, owner.public),
      jsonEncode({
        'v': 1,
        'request_id': requestId ?? request['request_id'],
        'status': status,
        'response': ?response,
      }),
    );
    listener?.call(
      signed(signingKey ?? '11' * 32, 24210, content, [
        ['p', owner.public],
        ['e', eventId ?? requestEvent.id],
        ['expiration', '${now + 120}'],
      ]),
    );
  }
}

class LaunchFixture {
  final LaunchRelay relay = LaunchRelay();
  String? stored;
  bool current = true;
  bool connected = true;
  bool denySave = false;
  void Function()? onSave;
  List<AgentLaunch> entries = [];
  final notices = <AgentLaunchNotice>[];
  void Function(List<AgentLaunch>)? onEntries;
  late final client = RemoteAgentStartClient(
    session: relay,
    config: RelayConfig(baseUrl: 'https://relay.example', nsec: owner.nsec),
    manager: manager.public,
    isCurrent: () => current,
  );
  late final queue = newQueue();

  MobileAgentLaunchQueue newQueue() => MobileAgentLaunchQueue(
    owner: owner.public,
    session: relay,
    load: () async => stored,
    save: (value) async {
      if (denySave) throw StateError('disk unavailable');
      stored = value;
      onSave?.call();
    },
    start: (agent, message, attempt) =>
        client.start(agent, message, attempt: attempt),
    isCurrent: () => current,
    isConnected: () => connected,
    onChanged: (value) {
      entries = value;
      onEntries?.call(value);
    },
    onLookupSkipped: notices.add,
  );

  SendMessage sender({
    void Function(String, NostrEvent)? addLocal,
    void Function(String, String)? removeLocal,
    void Function(String, String)? completeLocal,
  }) => SendMessage(
    signedEventRelay: SignedEventRelay(session: relay, nsec: owner.nsec),
    fetchMembers: (_) async => [
      ChannelMember(
        pubkey: agent.public,
        role: 'bot',
        joinedAt: DateTime(2025),
      ),
    ],
    readUserCache: () => const {},
    addLocalMessage: addLocal ?? (_, _) {},
    completeLocalMessage: completeLocal ?? (_, _) {},
    removeLocalMessage: removeLocal ?? (_, _) {},
    prepareAgentLaunch: queue.prepare,
    acceptAgentLaunch: queue.accepted,
    failAgentLaunch: queue.failed,
    isDeliveryValid: () => current,
  );

  Future<void> settle(bool Function() predicate) async {
    for (var i = 0; i < 100; i++) {
      if (predicate()) return;
      await Future<void>.delayed(const Duration(milliseconds: 2));
    }
    throw StateError('Launch did not settle');
  }

  void dispose() {
    current = false;
    client.dispose();
  }
}
