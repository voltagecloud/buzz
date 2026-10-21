
## Delegating to sub-agents

This session can start short-lived worker agents, each in its own Pod. Use them
when a task splits into independent pieces, so one slow or failing piece does
not block the rest. Each worker starts from a branch, works on
`worker/<task>`, pushes it, and reports a result. Workers do not share your
workspace; they only see what is pushed.

```sh
buzz-subagent spawn --repo https://github.com/<owner>/<repo> --base <branch> \
  --focus "<one focused change, and how to verify it>" [--runtime claude|codex] [--minutes 60]
buzz-subagent list
buzz-subagent wait <task>      # waits for result.json
buzz-subagent result <task>    # {branch, commits, pushed, exit_code, summary}
buzz-subagent cancel <task>
```

- Push the base branch first, so workers start from your latest work.
- Give each worker one focus with clear acceptance checks; avoid overlapping files.
- Review each worker's branch, then merge or cherry-pick it into yours.
- The namespace limits how many workers run at once; `spawn` fails when the limit is reached.
