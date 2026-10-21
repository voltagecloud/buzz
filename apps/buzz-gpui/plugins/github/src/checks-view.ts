// A pull request's Checks tab (buzz-app `GitHubChecks.tsx`): a ring of
// outcomes and its summary, then the checks grouped by outcome, each
// group opening and closing.

import { type Element, el, type Tone } from "@buzz/plugin-sdk";
import type { CheckDetail, CheckSummary } from "./checks.ts";

export const loading: CheckSummary = {
  state: "neutral",
  label: "Loading…",
  description: "Loading checks on the PR head commit.",
};

const summaryIcons: Record<CheckSummary["state"], { name: string; tone: Tone }> = {
  failure: { name: "XCircle", tone: "danger" },
  pending: { name: "WarningCircle", tone: "warning" },
  success: { name: "CheckCircle", tone: "success" },
  neutral: { name: "CircleDashed", tone: "subtle" },
  unavailable: { name: "WarningCircle", tone: "subtle" },
};

const checkIcons: Record<CheckDetail["category"], { name: string; tone: Tone }> = {
  failing: { name: "XCircle", tone: "danger" },
  cancelled: { name: "XCircle", tone: "danger" },
  pending: { name: "WarningCircle", tone: "warning" },
  unknown: { name: "WarningCircle", tone: "subtle" },
  successful: { name: "Check", tone: "success" },
  skipped: { name: "CircleDashed", tone: "subtle" },
  neutral: { name: "CircleDashed", tone: "subtle" },
};

export const groups: { category: CheckDetail["category"]; label: string }[] = [
  { category: "failing", label: "failing" },
  { category: "cancelled", label: "cancelled" },
  { category: "pending", label: "pending" },
  { category: "unknown", label: "unknown" },
  { category: "skipped", label: "skipped" },
  { category: "neutral", label: "neutral" },
  { category: "successful", label: "successful" },
];

const ringTone = (category: CheckDetail["category"]): Tone =>
  category === "failing" || category === "cancelled"
    ? "danger"
    : category === "pending"
      ? "warning"
      : category === "successful"
        ? "success"
        : "default";

export const needsRetry = (result: CheckSummary) =>
  result.state === "unavailable" || !!result.checks?.some((check) => check.category === "unknown");

export interface ChecksState {
  result: CheckSummary;
  attempt: number;
  /** Groups the user closed or opened, by category; skipped starts closed. */
  closed: Set<string>;
  /** Check links by key, for Copy link. */
  links: Map<string, string>;
}

export function checksView(state: ChecksState, hasSha: boolean): Element {
  const { result } = state;
  const total = result.checks?.length ?? 0;
  const sections = groups
    .map((group) => ({ ...group, checks: result.checks?.filter((check) => check.category === group.category) ?? [] }))
    .filter((group) => group.checks.length);
  const icon = summaryIcons[result.state];
  const isLoading = result === loading;
  return el.column(
    [
      el.row(
        [
          total && result.state !== "unavailable"
            ? el.ring(sections.map((group) => ({ value: group.checks.length, tone: ringTone(group.category) })))
            : el.icon(icon.name, { size: 32, tone: icon.tone }),
          el.column(
            [
              el.text(result.state === "failure" ? "Some checks were not successful" : result.label, { style: "label" }),
              el.text(result.description, { style: "small", tone: "subtle" }),
            ],
            { gap: "none", grow: true },
          ),
        ],
        { gap: "md" },
      ),
      hasSha &&
        (needsRetry(result) || (state.attempt > 0 && isLoading)) &&
        el.row([el.button("checks-retry", "Retry", { size: "sm", variant: "ghost", loading: isLoading })]),
      ...sections.map((group) => {
        const open = !state.closed.has(group.category);
        return el.column(
          [
            el.button(
              `group:${group.category}`,
              `${group.checks.length} ${group.label} ${group.checks.length === 1 ? "check" : "checks"}`,
              { icon: open ? "CaretDown" : "CaretRight", variant: "ghost", size: "sm" },
            ),
            open &&
              el.column(
                group.checks.map((check) => {
                  const icon = checkIcons[check.category];
                  if (check.url) state.links.set(check.key, check.url);
                  return el.row(
                    [
                      el.icon(icon.name, { size: 18, tone: icon.tone }),
                      el.column(
                        [
                          el.row(
                            [
                              check.url ? el.link(check.name, check.url, { external: true }) : el.text(check.name),
                              el.text(check.label, { style: "small", tone: "subtle" }),
                            ],
                            { gap: "sm", wrap: true },
                          ),
                          check.description && el.text(check.description, { style: "small", tone: "subtle" }),
                        ],
                        { gap: "none", grow: true },
                      ),
                      check.url &&
                        el.menu(`check:${check.key}`, [{ value: "copy", label: "Copy link", icon: "Link" }], {
                          icon: "DotsThree",
                          tooltip: `Actions for ${check.name}`,
                        }),
                    ],
                    { gap: "sm" },
                  );
                }),
                { gap: "sm", padding: "sm" },
              ),
          ],
          { gap: "none" },
        );
      }),
    ],
    { gap: "md" },
  );
}
