# Programs with built-in TLS

!!! warning "Experimental"
    Off by default and may change. Tested against a mock server with Claude Code (Bun) on a VM, with AES-128-GCM,
    AES-256-GCM and ChaCha20-Poly1305, and in k3s pods. Not yet tested on arm64.

Some programs carry their own TLS library and export no TLS function, so the usual probes see none of their calls:
Bun builds such as Claude Code and OpenCode (BoringSSL), and Rust programs on rustls with aws-lc such as Codex and
Goose. With this setting the agent also probes the cipher those libraries encrypt each TLS record with.

Claude Code is recorded. Codex is not yet: it calls the API over a WebSocket, which is not parsed yet
([#322](https://github.com/honeybee-studio/honeybeepf-llm/issues/322)), and its HTTP fallback is recorded only
with `SSL_CERT_FILE` set ([#323](https://github.com/honeybee-studio/honeybeepf-llm/issues/323)).

```yaml
builtinProbes:
  llm:
    aead: true
```

Outside Kubernetes, set `BUILTIN_PROBES__LLM_AEAD=true`.

To read the memory of named programs only, list them (an extension is ignored, so `claude` matches `claude.exe`):

```yaml
builtinProbes:
  llm:
    aead: true
    aeadExecutables: [claude, codex]
```

Outside Kubernetes: `BUILTIN_PROBES__LLM_AEAD_EXECUTABLES=claude,codex`.

## How it works

1. The agent first reads the executable file of each new process, and of each running one when it starts. Only a
   file with BoringSSL or aws-lc in its read-only data (and, if set, one of the listed names) goes on. For those it
   reads the memory of the program's own data, not its heap, for the library's table of cipher functions. A table
   is found by its sizes (key, nonce, tag) followed by pointers into the program's code, so no symbols are needed.
2. It attaches uprobes to the table's encrypt (`seal`) and decrypt (`open`) functions. Other processes of the same
   program use the same probes.
3. Kprobes on `tcp_sendmsg` and `tcp_recvmsg` tie each record to its TCP connection, so the call gets the same
   connection and pod as any other.

Some tables are filled when the first connection uses their cipher, so a process is scanned again until one shows,
for at most about half an hour; after that, later processes of the same program are not read at all. The first scan
of a program reads its data, about 60 ms for a 240 MB binary; later scans read only the pages the process wrote, a
few milliseconds each. What is read, and for how long, is in the
[security review](../security-review.md#3-data-handling).

## Limits

| Limit | Effect |
|---|---|
| TLS 1.3 only | A connection over TLS 1.2, such as through an old TLS-inspecting proxy, is not recorded. Provider endpoints use TLS 1.3. |
| WebSocket | Not parsed yet, so Codex's default calls are not recorded. |
| Connections open before the probes attach | A connection that rustls opened earlier may lose its first request. |
| TCP kprobes | Once a table is found, every TCP send and receive on the node runs one map lookup. |
| Coverage metric | Pods of these programs still count as `tls="other"` in [coverage](coverage.md). |
