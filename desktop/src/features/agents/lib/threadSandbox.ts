import type { ManagedAgent } from "@/shared/api/types";

export function usesThreadSandbox(
  agent: Pick<ManagedAgent, "backend">,
): boolean {
  return (
    agent.backend?.type === "provider" &&
    agent.backend.id === "kubernetes" &&
    agent.backend.config.sandbox === true
  );
}
