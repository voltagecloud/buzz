import type { RelayEvent, SendChannelMessageResult } from "@/shared/api/types";
import type { RemoteAgentSession } from "@/shared/api/remoteAgentSession";
import { buildThreadReferenceTags, getThreadReference } from "./threading";

export type PublishedMessage = Pick<RelayEvent, "id" | "tags">;
/** The relay-accepted event, or `undefined` when the sender published nothing. */
export type PublishedMessageResult = PublishedMessage | undefined;

export { usesThreadSandbox } from "@/features/agents/lib/threadSandbox";

/** Rebuilds the thread references of an event known only by its send result. */
export function publishedChannelMessage(
  channelId: string,
  result: Pick<
    SendChannelMessageResult,
    "eventId" | "parentEventId" | "rootEventId"
  >,
): PublishedMessage {
  return {
    id: result.eventId,
    tags: buildThreadReferenceTags(
      channelId,
      result.parentEventId,
      result.rootEventId,
    ),
  };
}

export function publishedRemoteSession(
  message: PublishedMessageResult,
  channelId: string | null,
): RemoteAgentSession | undefined {
  if (!message || !channelId) return undefined;
  // Matches the harness: a reply's root (or lone parent), else the message.
  const threadRoot = (
    getThreadReference(message.tags).rootId ?? message.id
  ).toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(threadRoot)) return undefined;
  return { channelId, threadRoot };
}
