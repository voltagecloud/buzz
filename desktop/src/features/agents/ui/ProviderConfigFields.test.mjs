import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { coerceConfigValues } from "./ProviderConfigFields.tsx";

const schema = {
  properties: {
    inactivity_seconds: { type: "integer" },
    threshold: { type: "number" },
    label: { type: "string" },
  },
};

describe("coerceConfigValues", () => {
  it("keeps deferred thread scope settings boolean before launch binds them", () => {
    const deferred = {
      properties: {
        sandbox: {
          oneOf: [{ const: false }, { type: "object" }],
          "x-buzz-ui-type": "boolean",
          default: false,
        },
      },
    };
    for (const enabled of [true, false]) {
      assert.deepEqual(
        coerceConfigValues({ sandbox: String(enabled) }, deferred),
        { sandbox: enabled },
      );
    }
  });
  it("omits cleared numeric fields without losing explicit zero", () => {
    assert.deepEqual(
      coerceConfigValues(
        { inactivity_seconds: "", threshold: "0", label: "" },
        schema,
      ),
      { threshold: 0, label: "" },
    );
  });

  it("preserves nonempty invalid numeric input for provider validation", () => {
    assert.deepEqual(
      coerceConfigValues({ inactivity_seconds: "not-a-number" }, schema),
      { inactivity_seconds: "not-a-number" },
    );
  });
});
