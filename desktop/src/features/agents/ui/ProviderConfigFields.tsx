import { Input } from "@/shared/ui/input";

/// Coerce string config values to their schema-declared types (number, boolean).
/// Providers receive JSON — sending "3" instead of 3 for an integer field breaks
/// typed config parsing on the provider side.
export function coerceConfigValues(
  config: Record<string, string>,
  schema: Record<string, unknown> | undefined,
): Record<string, unknown> {
  if (!schema) return { ...config };
  const properties = ((schema as Record<string, unknown>)?.properties ??
    {}) as Record<string, Record<string, unknown>>;
  const result: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(config)) {
    const prop = properties[key] as Record<string, unknown> | undefined;
    // A deferred Sandbox scope is a boolean setting until launch binds the
    // published thread. Its provider wire schema still requires the object.
    const schemaType =
      prop?.["x-buzz-ui-type"] === "boolean" ? "boolean" : prop?.type;
    if (schemaType === "integer" || schemaType === "number") {
      if (value === "") continue;
      const num = Number(value);
      result[key] = Number.isNaN(num) ? value : num;
    } else if (schemaType === "boolean") {
      result[key] = value === "true";
    } else if (schemaType === "object" || schemaType === "array") {
      // Submit is blocked while `invalidJsonConfigKeys` reports this field.
      const parsed = parseJsonField(value, schemaType);
      if (parsed !== undefined) result[key] = parsed;
    } else {
      result[key] = value;
    }
  }
  return result;
}

function parseJsonField(value: string, schemaType: "object" | "array") {
  if (value.trim() === "") return undefined;
  let parsed: unknown;
  try {
    parsed = JSON.parse(value);
  } catch {
    return undefined;
  }
  const isArray = Array.isArray(parsed);
  const isObject = parsed !== null && typeof parsed === "object" && !isArray;
  return (schemaType === "array" ? isArray : isObject) ? parsed : undefined;
}

/** Object/array fields whose nonempty input is not JSON of the declared shape. */
export function invalidJsonConfigKeys(
  config: Record<string, string>,
  schema: Record<string, unknown> | undefined,
): string[] {
  const properties = (schema?.properties ?? {}) as Record<
    string,
    Record<string, unknown>
  >;
  return Object.entries(config)
    .filter(([key, value]) => {
      const schemaType = properties[key]?.type;
      return (
        (schemaType === "object" || schemaType === "array") &&
        value.trim() !== "" &&
        parseJsonField(value, schemaType) === undefined
      );
    })
    .map(([key]) => key);
}

export function ProviderConfigFields({
  schema,
  config,
  onChange,
}: {
  schema: Record<string, unknown>;
  config: Record<string, string>;
  onChange: (config: Record<string, string>) => void;
}) {
  const properties = (schema as Record<string, unknown>)?.properties ?? {};
  const required = new Set<string>(
    ((schema as Record<string, unknown>)?.required as string[]) ?? [],
  );

  const invalidKeys = new Set(invalidJsonConfigKeys(config, schema));
  const entries = Object.entries(properties) as [
    string,
    Record<string, unknown>,
  ][];

  if (entries.length === 0) {
    return null;
  }

  return (
    <div className="space-y-3">
      {entries.map(([key, prop]) => (
        <div key={key} className="space-y-1.5">
          <label
            className="text-sm font-medium"
            htmlFor={`provider-cfg-${key}`}
          >
            {typeof prop.title === "string" ? prop.title : key}
            {required.has(key) ? (
              <span className="ml-1 text-destructive">*</span>
            ) : null}
          </label>
          <Input
            aria-describedby={
              invalidKeys.has(key) ? `provider-cfg-${key}-error` : undefined
            }
            aria-invalid={invalidKeys.has(key) || undefined}
            id={`provider-cfg-${key}`}
            onChange={(e) => onChange({ ...config, [key]: e.target.value })}
            placeholder={
              typeof prop.description === "string" ? prop.description : ""
            }
            value={
              config[key] ??
              (typeof prop.default === "string" ? prop.default : "")
            }
          />
          {typeof prop.description === "string" ? (
            <p className="text-xs text-muted-foreground">{prop.description}</p>
          ) : null}
          {invalidKeys.has(key) ? (
            <p
              className="text-xs text-destructive"
              id={`provider-cfg-${key}-error`}
            >
              Enter a valid JSON {prop.type === "array" ? "array" : "object"}.
            </p>
          ) : null}
        </div>
      ))}
    </div>
  );
}
