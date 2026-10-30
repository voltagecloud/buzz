# Move existing agents to Remote workspace

The native Agents page can move a saved team or several existing agents to Remote workspace.
This flow is available in builds with the `voltage-sandbox` feature.
It changes each selected agent's type through the existing single-agent operation.

## Move a team or a selection

1. Open **Agents → Your agents → Move to remote**.
2. Select a saved team, select individual agents, or choose **Select all agents**.
3. Choose **New remote configuration** or use an existing remote agent's configuration.
4. If needed, open **Configure remote workspace**, enroll your workspace profile, and save the destination draft.
5. Select **Review migration** and check each agent's harness, model, and local settings.
6. Select **Move selected agents**.

Alternatively, open **Teams** and select **Move to remote** beside a team.
The dialog preselects that team's members.
Each batch supports up to 32 agents.
Agents already using Remote workspace remain unchanged.

| Setting | Result |
| --- | --- |
| Key, name, picture, channel memberships, team membership | Preserved by the existing identity |
| Agent instructions | Carried through the type's migration policy |
| Claude/Codex harness, model, effort, fast mode | Preserved per agent |
| Other agent types | Use the reviewed destination harness and model options |
| Attention settings | Preserved; remote execution follows the remote type's supported triggers |
| Local workspace files and conversation sessions | Remain local |
| Local logins and agent environment variables | Not copied; enroll credentials in the remote workspace profile |

Moving an agent configures future remote sessions.
A mention starts a remote sandbox through the existing Sandbox manager.
Moving does not upload a local working directory or copy an existing conversation session.

## Resume interrupted work

Changes run sequentially.
The app saves a private migration record before changing an agent.
If an agent is busy or a host request fails, the batch stops and retains the remaining work.

Reopen **Move to remote** in the same account and community.
Select **Resume migration** after resolving the reported problem.
If a host completed a change before its response was lost, the app recognizes the exact destination configuration.
It does not repeat that change.

If a source, reference configuration, or selected team roster changed, select **Cancel remaining and review again**.
Completed changes remain in place.
The new review uses the current inventory.

If the saved migration record is unreadable, select **Archive unreadable record and start a fresh review**.
The app preserves the old record beside its data files and allows another review.
This action does not change any agent.

## Validation before review readiness

Use a separate test profile with a mixed Claude/Codex team and one already-remote agent.
Exercise both the whole-team and individual-selection paths.
Check that identities, memberships, instructions, and per-agent model options remain intact.
Interrupt a batch, reopen the dialog, and resume it.
Change one source after review and verify that the batch stops before changing that source.

A human must exercise the flow and confirm the results before this change becomes ready for review.
