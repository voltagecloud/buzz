import { describe, expect, it } from "vitest";
import { el } from "./index.ts";
import { FakeHost, texts } from "./testing.ts";

describe("serve", () => {
  it("answers initialize, draws views and redraws after events", async () => {
    let closed = 0;
    const host = new FakeHost({
      contributions: { pages: [{ id: "home", title: "Home" }] },
      views: {
        home: () => {
          let count = 0;
          return {
            render: () => el.column([el.text(`Clicked ${count}`), el.button("more", "More")]),
            event: (event) => {
              if (event.id === "more") count++;
            },
            close: () => closed++,
          };
        },
      },
    });
    expect(await host.initialize()).toEqual({ pages: [{ id: "home", title: "Home" }] });
    const view = await host.open("home");
    expect(texts(host.tree(view))).toEqual(["Clicked 0"]);
    await host.event(view, "more", "click");
    await host.event(view, "more", "click");
    expect(texts(host.tree(view))).toEqual(["Clicked 2"]);
    await host.notify("view/close", { view });
    expect(closed).toBe(1);
  });

  it("lets a view ask Buzz and redraw when the answer comes", async () => {
    const host = new FakeHost({
      contributions: {},
      views: {
        home: (_, view) => {
          let name = "…";
          void view.host.request<{ name: string }>("community/current").then((answer) => {
            name = answer.name;
            view.update();
          });
          return { render: () => el.text(name) };
        },
      },
    }).answer("community/current", () => ({ name: "Hive" }));
    const view = await host.open("home");
    expect(texts(host.tree(view))).toEqual(["Hive"]);
  });

  it("leaves out empty children and undefined fields", () => {
    expect(el.column([el.text("a"), false, null, undefined], { gap: undefined })).toEqual({
      type: "column",
      children: [{ type: "text", text: "a" }],
    });
  });
});
