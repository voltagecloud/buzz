// GitHub (`bundled/github`): a panel beside the conversation for GitHub
// repository, pull request, issue and commit links.

import type { PluginSpec } from "@buzz/plugin-sdk";
import { GitHubPanel } from "./panel.ts";

/**
 * The links `parseGitHubReference` reads, so others open in the browser:
 * a repository, a pull request (with a comment or review fragment), an
 * issue, or a commit.
 */
export const LINKS =
  "^https://(?i:github\\.com)(?::443)?/+[A-Za-z0-9_.-]+/+[A-Za-z0-9_.-]+" +
  "(?:/*(?:[?#].*)?" +
  "|/+pull/+[1-9][0-9]*/*(?:\\?[^#]*)?(?:#(?:issuecomment|pullrequestreview)-[0-9]+)?" +
  "|/+issues/+[1-9][0-9]*/*(?:\\?[^#]*)?" +
  "|/+commit/+[0-9A-Fa-f]{7,40}/*(?:[?#].*)?)$";

export const github: PluginSpec = {
  contributions: {
    panels: [{ id: "github", title: "GitHub", icon: "GithubLogo", pattern: LINKS }],
  },
  views: {
    github: (open, view) => {
      const panel = new GitHubPanel(view.host, String(open.target ?? ""), view.update);
      return {
        render: () => panel.render(),
        event: (event) => panel.event(event.id, event.event, event.value),
        retarget: (params) => panel.show(String(params.target ?? "")),
        close: () => panel.close(),
      };
    },
  },
};
