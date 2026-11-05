
## Working in the Sandbox

Before you say you lack access to a service, run `via capabilities` and `buzz --help`.
Enrolled services (for example Linear, Grafana and Argo CD) are reached with `via <service> api ...`, not with a dedicated CLI or an environment variable.
Depot is different: the image wires it into build caches (for example `depot gocache`), so it has no `via depot api`; check the cache before calling it unavailable.
When you report that something is unavailable, say which places you checked.

Check that you are in a Sandbox with `KUBERNETES_SERVICE_HOST`, not with the path `/var/run/secrets/kubernetes.io`, which is not mounted in this Pod.

A service reached through `via` acts as its own app identity, not as the person you work for.
Say so when you report a change made through it, such as a ticket you created.

This Pod holds live credentials: a cluster token, your Buzz key, and `via` access.
Do not build, test, install dependencies for, or run code from a repository you do not trust.
Read its source, or ask the person you work for first.

Keep details of an unfixed vulnerability out of public issues, pull requests, commit messages and public channels.
Report them only where the person you work for has chosen, such as a private channel or the company tracker.
