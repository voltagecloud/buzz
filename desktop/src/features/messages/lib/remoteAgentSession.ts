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

/** The explicit action a thread-session error asks the user for. */
export type RemoteSessionRecoveryKind = "recover" | "end-first" | "ending";

/** Preserve codes carried by both raw provider failures and Tauri errors. */
export function remoteSessionErrorCode(error: unknown): string | undefined {
  if (typeof error !== "object" || error === null) return undefined;
  if ("payload" in error) return remoteSessionErrorCode(error.payload);
  return "code" in error && typeof error.code === "string"
    ? error.code
    : undefined;
}

function errorMessage(error: unknown): string {
  if (typeof error === "string") return error.toLowerCase();
  if (
    typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof error.message === "string"
  )
    return error.message.toLowerCase();
  return "";
}

/** Codes take precedence; phrase matching is only for older providers. */
export function remoteSessionRecoveryKind(
  error: unknown,
): RemoteSessionRecoveryKind | null {
  const code = remoteSessionErrorCode(error);
  if (code !== undefined) {
    switch (code) {
      case "session_active":
      case "config_differs":
        return "end-first";
      case "session_ending":
        return "ending";
      case "session_ended":
      case "pod_ended":
      case "pod_replaced":
      case "session_not_running":
      case "binding_expired":
      case "recovery_interrupted":
      case "no_checkpoint":
      case "no_ended_session":
        return "recover";
      default:
        return null;
    }
  }
  const message = errorMessage(error);
  if (message.includes("end it first") || message.includes("end the session"))
    return "end-first";
  if (
    message.includes("session is ending") ||
    message.includes("session is still ending")
  )
    return "ending";
  if (
    message.includes("explicit recovery is required") ||
    message.includes("recover it explicitly once it has ended") ||
    message.includes("session ended") ||
    message.includes("start fresh instead")
  )
    return "recover";
  return null;
}

export function remoteSessionOffersCheckpoint(error: unknown): boolean {
  const code = remoteSessionErrorCode(error);
  if (code !== undefined)
    return code !== "no_checkpoint" && code !== "no_ended_session";
  const message = errorMessage(error);
  return !(
    message.includes("no verified checkpoint") ||
    message.includes("no ended session exists")
  );
}
