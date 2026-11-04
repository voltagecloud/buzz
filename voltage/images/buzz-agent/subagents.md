## Independent reviews

Use `buzz-subagent review` to review committed local work before publishing it.
Each review uses a fresh Codex or Claude session and a separate checkout.
It runs inside this Pod and does not consume a primary session or worker Pod.
Only one review can run at a time in this session.

```sh
buzz-subagent review --role quality --runtime codex --base origin/main
buzz-subagent review --role security --runtime claude --base origin/main
# Request a separate opinion when needed:
buzz-subagent review --role independent --runtime codex --base origin/main \
  --focus "Check cancellation after partial persistence."
```

Replace `origin/main` with the change's actual review base.
Commit local changes first; no push is required.
Run the repository's required checks separately.
Reviewers inspect code with read access; they do not run tests or build tools.
They receive the diff and review guidance from the base revision.

The command returns JSON with the reviewed `head`, target `base`, diff `merge_base`, findings, and `checks_not_run`.
Exit status is `0` for pass, `1` for blockers, `2` for command failure, or `3` for incomplete review.
A failed or incomplete review is not approval.
If the source changes during review, the command rejects the result.
If you change the implementation afterward, review the new commit.

Brokkr coordinates implementation and collects quality and security reviews.
These reviews use Tyr and Loki's specialties; the saved agents do not post replies.
Use Frigg for planning, Bifrost for another implementation, and Forseti for another opinion when needed.
Heimdall handles requested external PR reviews.
A direct mention of a saved agent still uses a primary session for that thread.
Do not wake every agent for a roll call.

## Implementation workers

This session can start short-lived worker agents, each in its own Pod.
Use workers for independent implementation tasks with separate acceptance checks.
Each worker starts from a published branch, works on `worker/<task>`, and pushes its branch.
Use this path only when the owner has authorized publishing that work.
Workers do not share your workspace; they only see published commits.

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
- The namespace limits concurrent workers; `spawn` fails when the limit is reached.
