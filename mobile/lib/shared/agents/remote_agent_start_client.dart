import 'dart:async';
import 'dart:convert';

import '../crypto/nip44.dart';
import '../relay/relay.dart';
import 'package:nostr/nostr.dart' as nostr;
import 'package:uuid/uuid.dart';

/// Same public manager identity used by desktop's Remote workspace type.
const remoteAgentManagerPubkey = String.fromEnvironment(
  'BUZZ_SANDBOX_MANAGER_PUBKEY',
  defaultValue:
      '1881c0c6186c81c00e607f9dcd9b6c655c49cd9a0227054e764a78e6829d31fd',
);

class AgentStartException implements Exception {
  final String message;
  const AgentStartException(this.message);
  @override
  String toString() => message;
}

/// Starts a registered agent over the existing encrypted session-control wire.
/// The phone signs as the owner; it never receives the agent's private key.
class RemoteAgentStartClient {
  final RelaySessionNotifier session;
  final RelayConfig config;
  final String manager;
  final bool Function() isCurrent;
  final Set<void Function()> _cancellations = {};
  final Duration acceptTimeout;
  final Duration finishTimeout;

  RemoteAgentStartClient({
    required this.session,
    required this.config,
    required this.isCurrent,
    this.manager = remoteAgentManagerPubkey,
    this.acceptTimeout = const Duration(seconds: 30),
    this.finishTimeout = const Duration(minutes: 10),
  });

  void dispose() {
    cancelActive();
  }

  /// Leave durable intents retryable when the relay connection is interrupted.
  void cancelActive() {
    for (final cancel in _cancellations.toList()) {
      cancel();
    }
    _cancellations.clear();
  }

  Future<void> start(
    String agent,
    NostrEvent trigger, {
    int attempt = 0,
  }) async {
    final nsec = config.nsec;
    final owner = pubkeyFromNsec(nsec);
    if (owner == null || !isCurrent() || trigger.pubkey != owner) {
      throw const AgentStartException(
        'Agent launch cancelled because the account changed.',
      );
    }
    final key = getConversationKey(
      nostr.Nip19.decode(payload: nsec!).data,
      manager,
    );
    final requestId = const Uuid().v5(
      Namespace.url.value,
      '${config.wsUrl}/$agent/${trigger.id}${attempt == 0 ? '' : '/$attempt'}',
    );
    final done = Completer<void>();
    final ready = Completer<void>();
    String? requestEvent;
    Timer? acceptTimer;
    void fail(String message) {
      if (!done.isCompleted) done.completeError(AgentStartException(message));
    }

    // Attach the error handler before subscribing, publishing or awaiting EOSE.
    void cancel() =>
        fail('Startup confirmation was interrupted. Retry the agent launch.');
    _cancellations.add(cancel);
    final result = done.future.timeout(
      finishTimeout,
      onTimeout: () => throw const AgentStartException(
        'The agent has not confirmed startup. Retry checks the same launch.',
      ),
    );
    result.ignore();
    void Function()? unsubscribe;
    try {
      unsubscribe = await session.subscribeWithStatus(
        NostrFilter(
          kinds: const [EventKind.sandboxSessionControl],
          authors: [manager],
          tags: {
            '#p': [owner],
          },
          limit: 0,
        ),
        (event) {
          if (!isCurrent() ||
              done.isCompleted ||
              event.pubkey != manager ||
              event.kind != EventKind.sandboxSessionControl) {
            return;
          }
          try {
            verifyAgentEvent(event);
            final tags = <String, String>{};
            for (final tag in event.tags) {
              if (tag.length != 2 ||
                  !const ['p', 'e', 'expiration'].contains(tag[0]) ||
                  tags.containsKey(tag[0])) {
                return;
              }
              tags[tag[0]] = tag[1];
            }
            final now = DateTime.now().millisecondsSinceEpoch ~/ 1000;
            if (tags['p'] != owner ||
                tags['e'] != requestEvent ||
                (now - event.createdAt).abs() > 60 ||
                (int.tryParse(tags['expiration'] ?? '') ?? 0) <= now) {
              return;
            }
            final reply =
                jsonDecode(nip44Decrypt(key, event.content))
                    as Map<String, dynamic>;
            if (reply['v'] != 1 || reply['request_id'] != requestId) return;
            if (reply['status'] == 'accepted' ||
                reply['status'] == 'progress') {
              acceptTimer?.cancel();
            } else if (reply['status'] == 'done') {
              acceptTimer?.cancel();
              final response = reply['response'];
              if (response is Map && response['ok'] == true) {
                done.complete();
              } else {
                fail(
                  _failureMessage(response is Map ? response['code'] : null),
                );
              }
            }
          } catch (_) {
            // Unrelated or malformed ciphertext never completes a launch.
          }
        },
        onClosed: (_) => fail(
          'The connection could not confirm startup. Retry the agent launch.',
        ),
        onStatusChanged: (status) {
          if (status == RelaySubscriptionStatus.ready && !ready.isCompleted) {
            ready.complete();
          }
          if (status == RelaySubscriptionStatus.retrying &&
              requestEvent != null) {
            fail('The connection was interrupted. Retry the agent launch.');
          }
        },
      );
      await Future.any<void>([ready.future.timeout(acceptTimeout), result]);
      if (!isCurrent()) {
        throw const AgentStartException(
          'Agent launch cancelled because the account changed.',
        );
      }
      if (done.isCompleted) return await result;
      final now = DateTime.now().millisecondsSinceEpoch ~/ 1000;
      acceptTimer = Timer(
        acceptTimeout,
        () => fail('The agent service did not answer. Retry the agent launch.'),
      );
      final published = SignedEventRelay(session: session, nsec: nsec).submit(
        kind: EventKind.sandboxSessionControl,
        createdAt: now,
        tags: [
          ['p', manager],
          ['expiration', '${now + 120}'],
        ],
        content: nip44Encrypt(
          key,
          jsonEncode({
            'v': 1,
            'op': 'start',
            'request_id': requestId,
            'payload': {
              'agent_pubkey': agent,
              'relay_url': config.wsUrl,
              'trigger': trigger.toJson(),
            },
          }),
        ),
        onSigned: (event) => requestEvent = event.id,
      );
      // A verified manager completion also proves delivery when the relay's
      // separate OK is delayed or lost. Still consume a later publish failure.
      await Future.any<void>([published.then<void>((_) {}), result]);
      await result;
    } on AgentStartException {
      rethrow;
    } catch (_) {
      throw const AgentStartException(
        'The connection could not confirm startup. Retry the agent launch.',
      );
    } finally {
      acceptTimer?.cancel();
      if (!ready.isCompleted) ready.complete();
      _cancellations.remove(cancel);
      unsubscribe?.call();
      // End the total deadline even when subscribe/publish failed first.
      if (!done.isCompleted) done.complete();
    }
  }
}

/// Verify received control events and owner listings before using their data.
void verifyAgentEvent(NostrEvent event) {
  nostr.Event(
    event.id,
    event.pubkey,
    event.createdAt,
    event.kind,
    event.tags,
    event.content,
    event.sig,
  );
}

String _failureMessage(Object? code) => switch (code) {
  'launch_not_registered' =>
    'Start this agent once from desktop after the server update to enable mobile launches.',
  'unsupported_op' =>
    'The agent service needs an update before it can accept mobile launches.',
  'unauthorized_owner' ||
  'signer_mismatch' => 'This account is not authorized to start the agent.',
  'session_ended' || 'session_ending' || 'recovery_interrupted' =>
    'This thread’s session ended. Recover it from desktop, or start a new thread.',
  'config_differs' =>
    'This thread is running with different settings. Continue there or start a new thread.',
  _ => 'The agent could not start. Retry the agent launch.',
};
