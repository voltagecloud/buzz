import 'package:flutter/material.dart';
import 'package:hooks_riverpod/hooks_riverpod.dart';

import '../theme/grid.dart';
import 'remote_agent_launch_provider.dart';

/// Startup status belongs to the channel/thread that requested it. Failed
/// starts offer retry independently of the already-sent message.
class RemoteAgentLaunchStatus extends ConsumerWidget {
  final String channelId;
  final String? threadRoot;
  const RemoteAgentLaunchStatus({
    super.key,
    required this.channelId,
    this.threadRoot,
  });

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final launches = ref.watch(remoteAgentLaunchesProvider);
    final notice = ref.watch(remoteAgentLaunchNoticeProvider);
    final showNotice =
        notice != null &&
        notice.message.channelId == channelId &&
        (threadRoot == null || notice.threadRoot == threadRoot);
    if (launches.hasError) {
      return ListTile(
        title: const Text('Agent startup history could not be loaded.'),
        trailing: TextButton(
          onPressed: () => ref.invalidate(remoteAgentLaunchesProvider),
          child: const Text('Retry'),
        ),
      );
    }
    final entries = (launches.value ?? [])
        .where(
          (entry) =>
              entry.message.channelId == channelId &&
              (threadRoot == null || entry.threadRoot == threadRoot),
        )
        .toList();
    if (entries.isEmpty && !showNotice) return const SizedBox.shrink();
    final queue = ref.read(remoteAgentLaunchesProvider.notifier).queue;
    return ConstrainedBox(
      constraints: const BoxConstraints(maxHeight: 180),
      child: SingleChildScrollView(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            if (showNotice)
              ListTile(
                leading: const Icon(Icons.info_outline),
                title: Text(notice.text),
                trailing: IconButton(
                  tooltip: 'Dismiss agent startup notice',
                  onPressed: () => ref
                      .read(remoteAgentLaunchNoticeProvider.notifier)
                      .dismiss(),
                  icon: const Icon(Icons.close),
                ),
              ),
            for (final entry in entries)
              Padding(
                padding: const EdgeInsets.symmetric(
                  horizontal: Grid.twelve,
                  vertical: Grid.half,
                ),
                child: Row(
                  children: [
                    if (entry.error == null)
                      const SizedBox(
                        width: 16,
                        height: 16,
                        child: CircularProgressIndicator(strokeWidth: 2),
                      )
                    else
                      const Icon(Icons.error_outline, size: 18),
                    const SizedBox(width: Grid.xxs),
                    Expanded(
                      child: Text(
                        entry.error == null
                            ? 'Starting ${entry.name}…'
                            : '${entry.name}: ${entry.error}',
                      ),
                    ),
                    if (entry.error != null) ...[
                      TextButton(
                        onPressed: () async {
                          try {
                            await queue?.retry(entry.id);
                          } catch (_) {
                            if (context.mounted) {
                              ScaffoldMessenger.of(context).showSnackBar(
                                const SnackBar(
                                  content: Text(
                                    'The launch retry could not be saved. Try again.',
                                  ),
                                ),
                              );
                            }
                          }
                        },
                        child: const Text('Retry'),
                      ),
                      IconButton(
                        tooltip: 'Dismiss startup for ${entry.name}',
                        onPressed: () async {
                          try {
                            await queue?.dismiss(entry.id);
                          } catch (_) {
                            if (context.mounted) {
                              ScaffoldMessenger.of(context).showSnackBar(
                                const SnackBar(
                                  content: Text(
                                    'The startup notice could not be dismissed. Try again.',
                                  ),
                                ),
                              );
                            }
                          }
                        },
                        icon: const Icon(Icons.close, size: 18),
                      ),
                    ],
                  ],
                ),
              ),
          ],
        ),
      ),
    );
  }
}
