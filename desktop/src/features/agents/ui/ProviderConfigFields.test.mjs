import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  coerceConfigValues,
  invalidJsonConfigKeys,
} from "./ProviderConfigFields.tsx";

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
  it("parses public scheduling objects and never submits invalid JSON", () => {
    const nested = { properties: { pod_options: { type: "object" } } };
    assert.deepEqual(
      coerceConfigValues(
        { pod_options: '{"node_selector":{"workload":"agents"}}' },
        nested,
      ),
      { pod_options: { node_selector: { workload: "agents" } } },
    );
    assert.deepEqual(coerceConfigValues({ pod_options: "" }, nested), {});
    assert.deepEqual(
      coerceConfigValues({ pod_options: "invalid" }, nested),
      {},
    );
  });
  it("reports object and array fields whose input is not JSON of that shape", () => {
    const nested = {
      properties: {
        pod_options: { type: "object" },
        tolerations: { type: "array" },
        label: { type: "string" },
      },
    };
    assert.deepEqual(
      invalidJsonConfigKeys(
        {
          pod_options: "{not json",
          tolerations: '{"key":"workload"}',
          label: "{not json",
        },
        nested,
      ),
      ["pod_options", "tolerations"],
    );
    assert.deepEqual(
      invalidJsonConfigKeys({ pod_options: "[]", tolerations: "null" }, nested),
      ["pod_options", "tolerations"],
    );
    assert.deepEqual(
      invalidJsonConfigKeys(
        { pod_options: " ", tolerations: '[{"key":"workload"}]' },
        nested,
      ),
      [],
    );
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
