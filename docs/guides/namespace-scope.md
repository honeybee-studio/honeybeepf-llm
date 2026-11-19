# Record only some namespaces

By default the agent records the LLM calls of every pod and process on its node. To limit it to some namespaces:

```yaml
builtinProbes:
  llm:
    namespaces: [team-a, team-b]
```

Outside the chart, set `BUILTIN_PROBES__LLM_NAMESPACES=team-a,team-b`.

| | Without `namespaces` | With `namespaces` |
|---|---|---|
| TLS data copied out of the kernel | from every process that uses a probed TLS library | only from the containers of pods in the listed namespaces |
| Calls recorded | every pod and host process | only those pods |
| Host processes (not in a pod) | recorded | never |

## How it works

1. The pod watcher finds the cgroup of each container of a pod in a listed namespace and adds it to a set in the
   kernel; it removes it when the pod is deleted.
2. Before a TLS probe copies any byte, it checks the cgroup of the calling thread against that set. A call from any
   other cgroup is not copied, only counted under `llm_events_total{stage="out_of_scope"}`.

The probes are still attached to every TLS library on the node, and the agent still reads each process's list of
libraries to do so; only the data is limited.

## Limits

| Limit | Effect |
|---|---|
| cgroup v2 only | On a cgroup v1 node pods cannot be told apart, so nothing is recorded; the log says so. |
| A new pod's first moments | A container is in scope once the pod watcher sees it, usually well under a second after it starts. A call made before that is not recorded. |
| No pod watcher | Without access to the Kubernetes API, no pod is known and nothing is recorded, rather than everything. |
| Plaintext capture | Not limited by namespace; it already captures only the endpoints of `plaintext` providers. |
