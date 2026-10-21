import { FakeHost, texts } from "@buzz/plugin-sdk/testing";
import { describe, expect, it } from "vitest";
import { bestie } from "./bestie.ts";

describe("bestie", () => {
  it("adds its page and greets", async () => {
    const host = new FakeHost(bestie);
    const answer = await host.initialize();
    expect(answer.pages).toEqual([{ id: "bestie", title: "Bestie", icon: "Bestie", layout: "workspace" }]);
    const view = await host.open("bestie");
    expect(texts(host.tree(view))).toEqual([
      "Bestie",
      "Meet your Bestie",
      "Your companion’s home in Buzz. Agent chat isn’t connected yet.",
    ]);
  });
});
