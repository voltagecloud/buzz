
## Staging cluster diagnostics

`kubectl` uses this Pod's temporary identity in `voltage-staging`.
The `KUBECONFIG` environment variable selects the generated configuration.
Kubelet rotates the mounted token; the configuration references its file.
No personal AWS profile or production credentials are needed.

Use these commands to investigate requests, scheduling, rollouts, and failures:

```sh
kubectl get pods -A -o wide
kubectl get events -A --sort-by=.metadata.creationTimestamp
kubectl get deployments,statefulsets,daemonsets -A
kubectl describe pod -n <namespace> <pod>
kubectl logs -n <namespace> <pod> -c <container> --since=30m --timestamps
kubectl logs -n <namespace> <pod> -c <container> --previous --tail=200
kubectl top nodes
kubectl top pods -A
kubectl get applications.argoproj.io -A
kubectl get sandboxes.agents.x-k8s.io -A
```

Cluster diagnostic permissions allow reads across staging namespaces.
They exclude Secrets, exec, attach, port forwarding, and node/service proxies.
Existing `buzz-subagent` permissions remain limited to this developer's worker claims.
Logs and manifests can contain sensitive information; quote only what explains the issue.
For older logs and historical metrics, use the enrolled Grafana service through `via`.
Report timestamps, namespaces, Pod names, and request/session IDs when explaining a failure.
