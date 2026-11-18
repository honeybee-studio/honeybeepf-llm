# Programs with built-in TLS

!!! warning "Experimental"
    Off by default and may change. Tested on a VM against a mock server with Claude Code (Bun) and Codex
    (rustls), with AES-128-GCM, AES-256-GCM and ChaCha20-Poly1305. Not yet tested on Kubernetes or arm64.

Some programs carry their own TLS library and export no TLS function, so the usual probes see none of their calls:
Bun builds such as Claude Code and OpenCode (BoringSSL), and Rust programs on rustls with aws-lc such as Codex and
Goose. With this setting the agent also probes the cipher those libraries encrypt each TLS record with.

```yaml
builtinProbes:
  llm:
    aead: true
```

Outside Kubernetes, set `BUILTIN_PROBES__LLM_AEAD=true`.

## How it works

1. The agent reads the memory of each new process, and of each running one when it starts, for the library's table
   of cipher functions. A table is found by its sizes (key, nonce, tag) followed by pointers into the program's
   code, so no symbols are needed.
2. It attaches uprobes to the table's encrypt (`seal`) and decrypt (`open`) functions. Other processes of the same
   program use the same probes.
3. Kprobes on `tcp_sendmsg` and `tcp_recvmsg` tie each record to its TCP connection, so the call gets the same
   connection and pod as any other.

Some tables are filled when the first connection uses their cipher, so a process is scanned again for about half an
hour. The first scan of a program reads its data, about 60 ms for a 240 MB binary; later scans read only the pages
the process wrote, a few milliseconds each.

## Limits

| Limit | Effect |
|---|---|
| TLS 1.3 only | A connection over TLS 1.2, such as through an old TLS-inspecting proxy, is not recorded. Provider endpoints use TLS 1.3. |
| Codex over WebSocket | Not parsed yet; Codex calls over HTTP/2 are recorded. |
| Connections open before the probes attach | A connection that rustls opened earlier may lose its first request. |
| TCP kprobes | Once a table is found, every TCP send and receive on the node runs one map lookup. |
| Coverage metric | Pods of these programs still count as `tls="other"` in [coverage](coverage.md). |
