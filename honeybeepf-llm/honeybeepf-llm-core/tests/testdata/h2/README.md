# HTTP/2 recordings

Plaintext HTTP/2 connections recorded on the server side of the hb-vm TLS mock (python-h2 4.3.0 server, ALPN
`h2`), one file per direction: `.c2s` is every byte the client sent (preface first), `.s2c` every byte the
server sent, each in order. Used by `tests/h2_frame.rs`.

## Files

Recorded 2026-10-30 on Ubuntu 26.04, kernel 7.0.0-38. A recording is not byte-reproducible across client
versions; regenerating means recording again with the same clients and replacing the rows.

| file | sha256 | command |
|---|---|---|
| curl-json.c2s.bin | 42b70e0cba34b42943282b91d5747c42db7a256af5fc15b6f555e5e1dff6f2a4 | `curl --http2 -X POST https://api.openai.com/v1/chat/completions -H 'X-Hb-Scenario: openai-json' -d '{"model":"gpt-4o","messages":[]}'` (curl 8.18.0, nghttp2 1.68.0) |
| curl-json.s2c.bin | 14bb484c9cfa1b6899bb6d44b60d06ce251cde6dcb36e7f190a715f7ff291173 | same connection, server side (239 B JSON body) |
| curl-sse.c2s.bin | bb3a35507e0b74112c8c5720d9c05c1a3219fa9ba17428c39fad9ab5d68a17e1 | as curl-json with `X-Hb-Scenario: openai-sse` and `"stream":true` |
| curl-sse.s2c.bin | ea85957940265c68a48ac830094a9bf32c04f311fb962cf884f45f5b351f2e25 | same connection, server side (306 B SSE, one DATA frame per event) |
| curl-large.c2s.bin | a73838881d12d7e74572742b2c1a7b56449afca711d7833fbabdf87628d03046 | as curl-json with `X-Hb-Scenario: openai-json-large` |
| curl-large.s2c.bin | ac93a63531411ddcfecc1d076ecfe8cd8788af07e1ea6bbefa5a99d12b36d904 | same connection, server side (40,237 B JSON in 16 KB DATA frames) |
| curl-gzip.c2s.bin | cbf688ddac2b51f6bd1cc692ce343d571a9fd7e95f65d55848ddfb9726eecae4 | as curl-json with `X-Hb-Scenario: openai-json-gzip` and `Accept-Encoding: gzip` |
| curl-gzip.s2c.bin | bc56be77cc0ad9236342e0cd63acd5848420de294d9640ff9a3c8d2cb0dcb868 | same connection, server side (172 B gzip body) |
| httpx-reuse.c2s.bin | 9e88ac5b3d79117856cc841a3016e83bc89bb60babd4ebe9c727d4246a08dd52 | `httpx.Client(http2=True)`, two POSTs on one connection, streams 1 and 3 (httpx 0.28.1, h2 4.3.0) |
| httpx-reuse.s2c.bin | 8d1ee3a693a94763ace82b8d0177d4ec3b283c05da9d6b17cbff911b0e145189 | same connection, server side |
| node26-openai-sse.c2s.bin | 651ef0178d5637797b81729355582f5dd2769c2ce8d295fab35e51a4d39d73c9 | `openai` 7.39.0 `chat.completions.create({stream: true})` on Node.js 26.13.0 (undici 8.12.2) |
| node26-openai-sse.s2c.bin | ea85957940265c68a48ac830094a9bf32c04f311fb962cf884f45f5b351f2e25 | same connection, server side (same bytes as curl-sse.s2c.bin) |
| node26-anthropic-json.c2s.bin | ada37bd74411e88027d831094c5ace49c5de55aedfe9f2905301d405f68ac53c | `@anthropic-ai/sdk` 0.135.0 `messages.create` on Node.js 26.13.0 |
| node26-anthropic-json.s2c.bin | 550dec9cc3b40a904244da9faf0237533d70bd0b7b6a6acaa2930f88b53848ff | same connection, server side |

The keys and hosts in the requests are fake (`Bearer fake`, `sk-hbvm-fake`, `sk-ant-hbvm-fake`); the hosts
resolve to the mock through `/etc/hosts`. The mock wraps the TLS socket's `recv` and `sendall` and appends each
call's bytes to the connection's two files.
