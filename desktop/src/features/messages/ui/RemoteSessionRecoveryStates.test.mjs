import assert from "node:assert/strict";
import { after, afterEach, test } from "node:test";
import { JSDOM } from "jsdom";
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
const React = await import("react");
const { cleanup, fireEvent, render, waitFor } = await import(
  "@testing-library/react"
);
const { QueryClient, QueryClientProvider } = await import(
  "@tanstack/react-query"
);
const { toast } = await import("sonner");
const { setDetachedToastScope } = await import("../lib/detachedToastScope.ts");
const { clearRemoteSessionRecoveryPrompt, recordRemoteSessionRecoveryPrompt } =
  await import("../lib/remoteSessionRecoveryPrompts.ts");
const { RemoteSessionRecoveryBanners } = await import(
  "./RemoteSessionRecoveryBanner.tsx"
);
after(() => dom.window.close());

const prompt = {
  agentPubkey: "a".repeat(64),
  agentName: "Thread agent",
  session: {
    channelId: "11111111-2222-3333-4444-555555555555",
    threadRoot: "b".repeat(64),
  },
  expectedRelayUrl: "wss://tenant-a.example",
  expectedSignerPubkey: "1".repeat(64),
  code: "session_active",
  error: "A session is active.",
};
let client;
afterEach(() => {
  cleanup();
  client?.clear();
  clearRemoteSessionRecoveryPrompt(prompt);
});

function mount(
  {
    supports = async () => true,
    end = async () => "ended",
    recover = async () => ({}),
  } = {},
  failure = {},
) {
  const calls = [];
  window.__TAURI_INTERNALS__ = globalThis.__TAURI_INTERNALS__ = {
    invoke(command, args) {
      calls.push({ command, args });
      if (command === "remote_agent_session_supports_lifecycle")
        return supports();
      if (command === "end_remote_agent_session") return end();
      if (command === "recover_remote_agent_session") return recover();
      return Promise.reject(new Error(`Unexpected command: ${command}`));
    },
    transformCallback: () => 1,
  };
  setDetachedToastScope({
    relayUrl: prompt.expectedRelayUrl,
    signerPubkey: prompt.expectedSignerPubkey,
  });
  recordRemoteSessionRecoveryPrompt({ ...prompt, ...failure });
  client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  return {
    ...render(
      React.createElement(
        QueryClientProvider,
        { client },
        React.createElement(RemoteSessionRecoveryBanners, {
          session: prompt.session,
        }),
      ),
    ),
    calls,
  };
}

for (const state of ["ending", "ended", "absent"]) {
  test(`end session renders the ${state} outcome`, async () => {
    let next = state;
    const view = mount({ end: async () => next });
    await waitFor(() => assert.ok(view.getByText("End session")));
    fireEvent.click(view.getByText("End session"));
    await waitFor(() => assert.ok(view.queryByText("End session") === null));
    if (state === "ending") {
      assert.ok(view.queryByText("Recover from checkpoint") === null);
      assert.ok(view.queryByText("Start fresh") === null);
      next = "ended";
      fireEvent.click(view.getByText("Check session"));
      await waitFor(() => assert.ok(view.getByText("Recover from checkpoint")));
    } else {
      assert.ok(view.getByText("Start fresh"));
      assert.equal(
        Boolean(view.queryByText("Recover from checkpoint")),
        state === "ended",
      );
    }
    for (const { args } of view.calls.filter(
      (c) => c.command === "end_remote_agent_session",
    )) {
      assert.equal(args.expectedRelayUrl, prompt.expectedRelayUrl);
      assert.equal(args.expectedSignerPubkey, prompt.expectedSignerPubkey);
    }
  });
}

test("typed recovery refusals retarget actions and suppress unavailable checkpoints", async () => {
  let failure = { code: "no_checkpoint", message: "No snapshot is available." };
  const view = mount(
    {
      recover: async () => {
        throw failure;
      },
    },
    { code: "session_ended" },
  );
  await waitFor(() => assert.ok(view.getByText("Recover from checkpoint")));
  fireEvent.click(view.getByText("Recover from checkpoint"));
  await waitFor(() =>
    assert.ok(view.queryByText("Recover from checkpoint") === null),
  );
  assert.ok(view.getByText("Start fresh"));
  failure = { code: "session_active", message: "A new session exists." };
  fireEvent.click(view.getByText("Start fresh"));
  await waitFor(() => assert.ok(view.getByText("End session")));
  assert.ok(view.queryByText("Start fresh") === null);
});

test("capability failures remain visible and can be retried or dismissed", async () => {
  let fail = true;
  const view = mount({
    supports: async () => {
      if (fail) throw new Error("Provider unavailable");
      return true;
    },
  });
  await waitFor(() => assert.ok(view.getByText("Retry")));
  assert.match(
    view.container.textContent,
    /Could not check remote session controls/,
  );
  assert.ok(view.queryByText("End session") === null);
  fail = false;
  fireEvent.click(view.getByText("Retry"));
  await waitFor(() => assert.ok(view.getByText("End session")));
  fireEvent.click(view.getByText("Dismiss"));
  await waitFor(() => assert.equal(view.container.textContent, ""));
});

test("unsupported providers offer no lifecycle actions", async () => {
  const view = mount({ supports: async () => false });
  await waitFor(() =>
    assert.equal(
      client.getQueryState([
        "remote-agent-session-lifecycle",
        prompt.agentPubkey,
      ])?.status,
      "success",
    ),
  );
  assert.equal(view.container.textContent, "");
});

for (const mode of ["checkpoint", "fresh"]) {
  test(`${mode} recovery sends one mode and reports the matching outcome`, async () => {
    const view = mount({}, { code: "session_ended" });
    await waitFor(() => assert.ok(view.getByText("Start fresh")));
    fireEvent.click(
      view.getByText(
        mode === "checkpoint" ? "Recover from checkpoint" : "Start fresh",
      ),
    );
    await waitFor(() => assert.equal(view.container.textContent, ""));
    const calls = view.calls.filter(
      (c) => c.command === "recover_remote_agent_session",
    );
    assert.equal(calls.length, 1);
    assert.equal(calls[0].args.mode, mode);
    assert.match(
      String(toast.getToasts().at(-1).title),
      mode === "checkpoint"
        ? /restoring its workspace/
        : /starting a fresh workspace/,
    );
  });
}
