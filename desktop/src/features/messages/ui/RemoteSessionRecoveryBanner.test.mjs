import assert from "node:assert/strict";
import { test } from "node:test";
import { JSDOM } from "jsdom";

test("recovery success remains fenced after a community switch", async () => {
  const dom = new JSDOM("<!doctype html><html><body></body></html>", {
    url: "http://localhost",
  });
  Object.assign(globalThis, {
    window: dom.window,
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    localStorage: dom.window.localStorage,
    IS_REACT_ACT_ENVIRONMENT: true,
  });
  let finishRecovery;
  dom.window.__TAURI_INTERNALS__ = {
    invoke: (command) => {
      if (command === "remote_agent_session_supports_lifecycle")
        return Promise.resolve(true);
      if (command === "recover_remote_agent_session")
        return new Promise((resolve) => {
          finishRecovery = resolve;
        });
      return Promise.reject(new Error(`unmocked command: ${command}`));
    },
    transformCallback: () => 1,
  };
  globalThis.__TAURI_INTERNALS__ = dom.window.__TAURI_INTERNALS__;
  const React = await import("react");
  const { render, act, fireEvent, waitFor } = await import(
    "@testing-library/react"
  );
  const { QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  );
  const { toast } = await import("sonner");
  const { setDetachedToastScope } = await import(
    "../lib/detachedToastScope.ts"
  );
  const { recordRemoteSessionRecoveryPrompt } = await import(
    "../lib/remoteSessionRecoveryPrompts.ts"
  );
  const { RemoteSessionRecoveryBanners } = await import(
    "./RemoteSessionRecoveryBanner.tsx"
  );
  const signer = "1".repeat(64);
  const pubkey = "a".repeat(64);
  const session = {
    channelId: "11111111-2222-3333-4444-555555555555",
    threadRoot: "b".repeat(64),
  };
  setDetachedToastScope({
    relayUrl: "wss://tenant-a.example",
    signerPubkey: signer,
  });
  recordRemoteSessionRecoveryPrompt({
    agentPubkey: pubkey,
    agentName: "Tenant A private agent",
    session,
    expectedRelayUrl: "wss://tenant-a.example",
    expectedSignerPubkey: signer,
    error: "Sandbox session ended; explicit recovery is required",
  });
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  const rendered = render(
    React.createElement(
      QueryClientProvider,
      { client },
      React.createElement(RemoteSessionRecoveryBanners, { session }),
    ),
  );
  try {
    await waitFor(() =>
      assert.ok(rendered.getByText("Recover from checkpoint")),
    );
    fireEvent.click(rendered.getByText("Recover from checkpoint"));
    assert.ok(finishRecovery);
    // Switching communities unmounts this subtree, but not the global Toaster.
    rendered.unmount();
    setDetachedToastScope({
      relayUrl: "wss://tenant-b.example",
      signerPubkey: signer,
    });
    await act(async () => {
      finishRecovery({
        pubkey,
        name: "Tenant A private agent",
        status: "deployed",
      });
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
    assert.deepEqual(
      toast.getToasts().map((entry) => String(entry.title ?? "")),
      [],
      "the previous community's agent name must not surface in the current community",
    );
  } finally {
    rendered.unmount();
    client.clear();
    dom.window.close();
  }
});
