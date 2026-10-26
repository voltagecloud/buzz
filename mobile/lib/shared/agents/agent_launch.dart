import '../relay/relay.dart';

String _threadRoot(NostrEvent message) {
  for (final marker in ['root', 'reply']) {
    for (final tag in message.tags) {
      if (tag.length >= 4 && tag[0] == 'e' && tag[3] == marker) return tag[1];
    }
  }
  return message.id;
}

/// A transient, community-scoped notice; it never authorizes an agent launch.
class AgentLaunchNotice {
  final NostrEvent message;
  final String text;
  const AgentLaunchNotice(this.message, this.text);
  String get threadRoot => _threadRoot(message);
}

/// A durable startup intent. It contains a signed message, never agent secrets.
class AgentLaunch {
  final String agent;
  final String name;
  final NostrEvent message;
  final bool accepted;

  /// A persisted generation for explicit retries; restarts reuse this value.
  final int attempt;
  final String? error;

  const AgentLaunch({
    required this.agent,
    required this.name,
    required this.message,
    this.accepted = false,
    this.attempt = 0,
    this.error,
  });

  String get id => '${message.id}:$agent';
  String get threadRoot => _threadRoot(message);

  AgentLaunch copyWith({bool? accepted, int? attempt, String? error}) =>
      AgentLaunch(
        agent: agent,
        name: name,
        message: message,
        accepted: accepted ?? this.accepted,
        attempt: attempt ?? this.attempt,
        error: error,
      );

  Map<String, dynamic> toJson() => {
    'agent': agent,
    'name': name,
    'message': message.toJson(),
    'accepted': accepted,
    'attempt': attempt,
    'error': error,
  };
  factory AgentLaunch.fromJson(Map<String, dynamic> value) => AgentLaunch(
    agent: value['agent'] as String,
    name: value['name'] as String,
    message: NostrEvent.fromJson(value['message'] as Map<String, dynamic>),
    accepted: value['accepted'] as bool,
    attempt: (value['attempt'] as int?) ?? 0,
    error: value['error'] as String?,
  );
}
