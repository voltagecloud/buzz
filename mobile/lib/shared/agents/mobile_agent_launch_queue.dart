import 'dart:async';
import 'dart:convert';

import '../relay/relay.dart';
import 'agent_launch.dart';
import 'remote_agent_start_client.dart';

/// Persists startup intents before message publication. Retries launch the
/// original signed message, never republish it or borrow another thread.
class MobileAgentLaunchQueue {
  final String owner;
  final RelaySessionNotifier session;
  final Future<String?> Function() load;
  final Future<void> Function(String) save;
  final Future<void> Function(String agent, NostrEvent message, int attempt)
  start;
  final bool Function() isCurrent;
  final bool Function() isConnected;
  final void Function(List<AgentLaunch>) onChanged;
  final void Function(AgentLaunchNotice)? onLookupSkipped;
  final Map<String, AgentLaunch> _entries = {};
  final Set<String> _running = {};
  final Set<String> _publishing = {};
  Future<void> _tail = Future.value();
  late final Future<void> ready = _restore();
  static const maxEntries = 32;
  static const maxConcurrent = 4;

  MobileAgentLaunchQueue({
    required this.owner,
    required this.session,
    required this.load,
    required this.save,
    required this.start,
    required this.isCurrent,
    required this.isConnected,
    required this.onChanged,
    this.onLookupSkipped,
  });

  Future<void> _restore() async {
    final text = await load();
    if (!isCurrent()) return;
    if (text != null) {
      final values = jsonDecode(text) as List;
      if (values.length > maxEntries) {
        throw const FormatException('Too many saved agent launches');
      }
      for (final value in values) {
        final entry = AgentLaunch.fromJson(value as Map<String, dynamic>);
        if (entry.attempt < 0) {
          throw const FormatException('Invalid launch attempt');
        }
        verifyAgentEvent(entry.message);
        if (entry.message.pubkey != owner) {
          throw const FormatException(
            'Saved launch belongs to another account',
          );
        }
        _entries[entry.id] = entry;
      }
    }
    _notify();
  }

  void _notify() {
    if (isCurrent()) onChanged(List.unmodifiable(_entries.values));
  }

  Future<void> _change(void Function(Map<String, AgentLaunch>) change) {
    final result = _tail.then((_) async {
      if (!isCurrent()) throw StateError('The active community changed');
      final next = Map<String, AgentLaunch>.from(_entries);
      change(next);
      await save(jsonEncode(next.values.map((e) => e.toJson()).toList()));
      if (!isCurrent()) return;
      _entries
        ..clear()
        ..addAll(next);
      _notify();
    });
    _tail = result.then<void>((_) {}, onError: (Object _, StackTrace _) {});
    return result;
  }

  /// Only owner-signed Remote workspace listings may request startup. A
  /// profile, bot role or another owner's listing cannot authorize a launch.
  Future<void> prepare(NostrEvent message) async {
    if (message.pubkey != owner) throw StateError('The active account changed');
    final recipients = message.tags
        .where((t) => t.length >= 2 && t[0] == 'p')
        .map((t) => t[1])
        .toSet();
    if (recipients.isEmpty) return;
    List<NostrEvent> records;
    try {
      records = await session
          .fetchHistory(
            NostrFilter(
              kinds: const [EventKind.managedAgent],
              authors: [owner],
              tags: {'#d': recipients.toList()},
              limit: recipients.length,
            ),
            timeout: const Duration(seconds: 2),
          )
          .timeout(const Duration(seconds: 2));
    } catch (_) {
      if (!isCurrent()) throw StateError('The active community changed');
      _lookupSkipped(message);
      return;
    }
    if (!isCurrent()) throw StateError('The active community changed');
    final newest = <String, NostrEvent>{};
    for (final record in records) {
      if (record.pubkey != owner || record.kind != EventKind.managedAgent) {
        continue;
      }
      try {
        verifyAgentEvent(record);
      } catch (_) {
        continue;
      }
      final agent = record.getTagValue('d');
      if (agent == null || !recipients.contains(agent)) continue;
      final prior = newest[agent];
      if (prior == null ||
          record.createdAt > prior.createdAt ||
          (record.createdAt == prior.createdAt &&
              record.id.compareTo(prior.id) < 0)) {
        newest[agent] = record;
      }
    }
    final launches = <AgentLaunch>[];
    for (final record in newest.values) {
      if (record.getTagValue('runtime') != 'voltage.remote-workspace/sandbox') {
        continue;
      }
      Map<String, dynamic> body;
      try {
        body = jsonDecode(record.content) as Map<String, dynamic>;
      } catch (_) {
        _lookupSkipped(message);
        continue;
      }
      if (utf8.encode(jsonEncode(message.toJson())).length > 60000) {
        throw ArgumentError(
          'This message is too large to start an agent. Shorten it and try again.',
        );
      }
      final agent = record.getTagValue('d')!;
      final name = body['name'] is String ? body['name'] as String : 'agent';
      launches.add(AgentLaunch(agent: agent, name: name, message: message));
    }
    if (launches.isEmpty) return;
    // Only a confirmed eligible launch depends on the durable journal.
    await ready;
    _publishing.add(message.id);
    try {
      await _change((entries) {
        for (final launch in launches) {
          entries[launch.id] = launch;
        }
        if (entries.length > maxEntries) {
          throw StateError(
            'Retry or dismiss previous agent launches before sending another.',
          );
        }
      });
    } catch (_) {
      _publishing.remove(message.id);
      rethrow;
    }
  }

  void _lookupSkipped(NostrEvent message) {
    if (isCurrent()) {
      onLookupSkipped?.call(
        AgentLaunchNotice(
          message,
          'Agent startup was skipped because its settings could not be checked.',
        ),
      );
    }
  }

  Future<void> accepted(NostrEvent message) async {
    try {
      if (!message.tags.any((tag) => tag.isNotEmpty && tag[0] == 'p')) return;
      if (!_entries.values.any((entry) => entry.message.id == message.id)) {
        return;
      }
      await ready;
      try {
        await _change((entries) {
          for (final entry in entries.values.toList()) {
            if (entry.message.id == message.id) {
              entries[entry.id] = entry.copyWith(accepted: true);
            }
          }
        });
      } catch (_) {
        // The pre-publication intent is already durable. A restart/retry checks
        // relay acceptance instead of losing or republishing the sent message.
        _showFailure(
          message.id,
          'Message sent. Startup confirmation could not be saved; retry the agent launch.',
        );
      }
    } finally {
      _publishing.remove(message.id);
    }
    resume();
  }

  Future<void> failed(NostrEvent message) async {
    try {
      if (!message.tags.any((tag) => tag.isNotEmpty && tag[0] == 'p')) return;
      if (!_entries.values.any((entry) => entry.message.id == message.id)) {
        return;
      }
      await ready;
      const error =
          'Message delivery was not confirmed. Retry checks whether it reached the relay.';
      try {
        await _change((entries) {
          for (final entry in entries.values.toList()) {
            if (entry.message.id == message.id) {
              entries[entry.id] = entry.copyWith(error: error);
            }
          }
        });
      } catch (_) {
        _showFailure(message.id, error);
      }
    } finally {
      _publishing.remove(message.id);
    }
  }

  void _showFailure(String messageId, String error) {
    if (!isCurrent()) return;
    for (final entry in _entries.values.toList()) {
      if (entry.message.id == messageId) {
        _entries[entry.id] = entry.copyWith(error: error);
      }
    }
    _notify();
  }

  /// Resume interrupted pending launches after reconnect or process restart.
  /// Rejections remain visible until the user explicitly retries or dismisses.
  void resume() {
    if (!isCurrent() || !isConnected()) return;
    for (final entry in _entries.values.toList()) {
      if (_running.length >= maxConcurrent) break;
      if (entry.error != null ||
          _running.contains(entry.id) ||
          _publishing.contains(entry.message.id)) {
        continue;
      }
      _running.add(entry.id);
      unawaited(_run(entry));
    }
  }

  Future<void> retry(String id) async {
    await ready;
    if (_running.contains(id)) return;
    await _change((entries) {
      final entry = entries[id];
      if (entry != null) {
        entries[id] = entry.copyWith(attempt: entry.attempt + 1);
      }
    });
    resume();
  }

  Future<void> dismiss(String id) async {
    if (_running.contains(id)) return;
    await _change((entries) => entries.remove(id));
  }

  Future<void> _run(AgentLaunch entry) async {
    try {
      if (!entry.accepted) {
        final events = await session.fetchHistory(
          NostrFilter(
            kinds: const [9, 40002],
            ids: [entry.message.id],
            limit: 1,
          ),
        );
        final accepted = events.any((event) {
          verifyAgentEvent(event);
          return event.id == entry.message.id && event.pubkey == owner;
        });
        if (!accepted) {
          throw const AgentStartException(
            'The message was not found on the relay. Send the message again to start the agent.',
          );
        }
      }
      if (!isCurrent() || !isConnected()) return;
      await start(entry.agent, entry.message, entry.attempt);
      await _change((entries) => entries.remove(entry.id));
    } catch (error) {
      if (!isCurrent()) return;
      final message = error is AgentStartException
          ? error.message
          : 'Startup could not be confirmed. Retry the agent launch.';
      try {
        await _change((entries) {
          final current = entries[entry.id];
          if (current != null) {
            entries[entry.id] = current.copyWith(error: message);
          }
        });
      } catch (_) {
        _showFailure(entry.message.id, message);
      }
    } finally {
      _running.remove(entry.id);
      resume();
    }
  }
}
