import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:hooks_riverpod/hooks_riverpod.dart';
import 'package:pointycastle/digests/sha256.dart';
import 'package:shared_preferences/shared_preferences.dart';

import '../relay/relay.dart';
import 'agent_launch.dart';
import 'mobile_agent_launch_queue.dart';
import 'remote_agent_start_client.dart';

/// Include the full endpoint so communities sharing a host keep separate queues.
String remoteAgentLaunchStorageKey(RelayConfig config, String owner) {
  final digest = SHA256Digest()
      .process(Uint8List.fromList(utf8.encode(config.wsUrl)))
      .map((byte) => byte.toRadixString(16).padLeft(2, '0'))
      .join();
  return 'mobile-agent-launches:v1:$digest:$owner';
}

/// A dismissible notice that cannot prevent ordinary message publication.
class RemoteAgentLaunchNotice extends Notifier<AgentLaunchNotice?> {
  @override
  AgentLaunchNotice? build() {
    ref.watch(relayConfigProvider);
    return null;
  }

  /// Show the most recent lookup failure in its originating conversation.
  void show(AgentLaunchNotice notice) => state = notice;

  /// Dismiss the transient notice.
  void dismiss() => state = null;
}

final remoteAgentLaunchNoticeProvider =
    NotifierProvider<RemoteAgentLaunchNotice, AgentLaunchNotice?>(
      RemoteAgentLaunchNotice.new,
    );

class RemoteAgentLaunches extends Notifier<AsyncValue<List<AgentLaunch>>> {
  MobileAgentLaunchQueue? queue;

  @override
  AsyncValue<List<AgentLaunch>> build() {
    final config = ref.watch(relayConfigProvider);
    final owner = pubkeyFromNsec(config.nsec);
    if (owner == null) {
      queue = null;
      return const AsyncData([]);
    }
    final session = ref.read(relaySessionProvider.notifier);
    var active = true;
    bool current() =>
        active &&
        ref.mounted &&
        ref.read(relayConfigProvider).baseUrl == config.baseUrl &&
        ref.read(relayConfigProvider).nsec == config.nsec;
    final client = RemoteAgentStartClient(
      session: session,
      config: config,
      isCurrent: current,
    );
    ref.onDispose(() {
      active = false;
      client.dispose();
    });
    final storageKey = remoteAgentLaunchStorageKey(config, owner);
    final localQueue = MobileAgentLaunchQueue(
      owner: owner,
      session: session,
      isCurrent: current,
      isConnected: () =>
          ref.read(relaySessionProvider).status == SessionStatus.connected,
      start: (agent, message, attempt) =>
          client.start(agent, message, attempt: attempt),
      onLookupSkipped: (notice) {
        if (current()) {
          ref.read(remoteAgentLaunchNoticeProvider.notifier).show(notice);
        }
      },
      load: () async =>
          (await SharedPreferences.getInstance()).getString(storageKey),
      save: (value) async {
        final saved = await (await SharedPreferences.getInstance()).setString(
          storageKey,
          value,
        );
        if (!saved) throw StateError('Agent launch history could not be saved');
      },
      onChanged: (entries) {
        if (current()) state = AsyncData(entries);
      },
    );
    queue = localQueue;
    ref.listen(relaySessionProvider.select((s) => s.status), (_, status) {
      if (status != SessionStatus.connected) client.cancelActive();
      if (status == SessionStatus.connected) {
        unawaited(
          localQueue.ready.then(
            (_) {
              if (current()) localQueue.resume();
            },
            onError: (Object e, StackTrace st) {
              if (current()) state = AsyncError(e, st);
            },
          ),
        );
      }
    });
    unawaited(
      localQueue.ready.then(
        (_) {
          if (current() &&
              ref.read(relaySessionProvider).status ==
                  SessionStatus.connected) {
            localQueue.resume();
          }
        },
        onError: (Object e, StackTrace st) {
          if (current()) state = AsyncError(e, st);
        },
      ),
    );
    return const AsyncLoading();
  }
}

final remoteAgentLaunchesProvider =
    NotifierProvider<RemoteAgentLaunches, AsyncValue<List<AgentLaunch>>>(
      RemoteAgentLaunches.new,
    );
