// Times as buzz-app shows them (`shared/relative-timestamp.ts`).

/** Matches Buzz desktop's thread-summary recency ladder (dateFormatters.ts). */
export function relativeTimestamp(unixSeconds: number, nowSeconds = Date.now() / 1000) {
  const age = Math.max(0, nowSeconds - unixSeconds);
  const ago = (value: number, unit: string) => `${value} ${unit}${value === 1 ? "" : "s"} ago`;
  if (age < 60) return "just now";
  if (age < 3600) return ago(Math.floor(age / 60), "minute");
  if (age < 86400) return ago(Math.floor(age / 3600), "hour");
  if (age < 604800) return ago(Math.floor(age / 86400), "day");
  return `on ${new Date(unixSeconds * 1000).toLocaleDateString("en-US", { month: "short", day: "numeric" })}`;
}

/** "3 hours ago", for an ISO time. */
export const relative = (value: string) => relativeTimestamp(Date.parse(value) / 1000);

/** The local date and time, for a time's tooltip (`toLocaleString()`). */
export const local = (value: string) => new Date(value).toLocaleString("en-US");
