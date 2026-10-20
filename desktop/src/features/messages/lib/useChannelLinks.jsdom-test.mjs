import assert from "node:assert/strict";
import { test } from "node:test";
import React, { act } from "react";
import { createRoot } from "react-dom/client";

import { ChannelNavigationProvider } from "@/shared/context/ChannelNavigationContext";
import { useChannelLinks } from "./useChannelLinks.ts";

const channels = [
  {
    id: "general",
    name: "general",
    channelType: "stream",
    archivedAt: null,
  },
];

function mountChannelLinks(t) {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const root = createRoot(document.createElement("div"));
  let current;
  function Harness() {
    current = useChannelLinks();
    return null;
  }
  act(() => {
    root.render(
      React.createElement(
        ChannelNavigationProvider,
        { channels },
        React.createElement(Harness),
      ),
    );
  });
  t.after(() => act(() => root.unmount()));
  return () => current;
}

for (const replacement of ["", "Edited message", "Welcome to #general "]) {
  test(`channel suggestions close immediately after replacing the query with ${JSON.stringify(replacement)}`, (t) => {
    const links = mountChannelLinks(t);
    act(() => links().updateChannelQuery("Welcome to #general", 19));
    act(() => t.mock.timers.tick(120));
    assert.equal(links().isChannelOpen, true);

    act(() => links().updateChannelQuery(replacement, replacement.length));

    // Enter must submit the current edit before the debounce window ends.
    assert.equal(links().isChannelOpen, false);
    for (const key of ["Enter", "Tab"]) {
      assert.deepEqual(
        links().handleChannelKeyDown({
          key,
          preventDefault: () =>
            assert.fail("stale suggestion consumed the key"),
        }),
        { handled: false },
      );
    }
    act(() => t.mock.timers.tick(120));
    assert.equal(links().isChannelOpen, false);
  });
}

test("clearing a pending query cancels it; a later valid query still opens", (t) => {
  const links = mountChannelLinks(t);
  act(() => links().updateChannelQuery("#gen", 4));
  assert.equal(links().isChannelOpen, false);
  act(() => links().updateChannelQuery("", 0));
  act(() => t.mock.timers.tick(120));
  assert.equal(links().isChannelOpen, false);

  act(() => links().updateChannelQuery("#gen", 4));
  act(() => t.mock.timers.tick(120));
  assert.equal(links().isChannelOpen, true);
  const event = { key: "Enter", preventDefault: t.mock.fn() };
  assert.deepEqual(links().handleChannelKeyDown(event), {
    handled: true,
    suggestion: { id: "general", name: "general", channelType: "stream" },
  });
  assert.equal(event.preventDefault.mock.callCount(), 1);
});
