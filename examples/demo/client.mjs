// Demo LLM client on the official Node.js image (OpenSSL built into the node binary): `node client.mjs <seconds>`
// alternates Gemini JSON and SSE calls through fetch. It trusts the demo CA through NODE_EXTRA_CA_CERTS and
// uses a fake key.
const every = Number(process.argv[2] ?? 6) * 1000;
const base = "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash";
const calls = [
  ["gemini-json", `${base}:generateContent`],
  ["gemini-sse", `${base}:streamGenerateContent?alt=sse`],
];
for (let i = 0; ; i++) {
  const [response, url] = calls[i % calls.length];
  try {
    const r = await fetch(url, {
      method: "POST",
      headers: { "X-Demo-Response": response, "x-goog-api-key": "demo-not-a-real-key", "Content-Type": "application/json" },
      body: JSON.stringify({ contents: [{ parts: [{ text: "summarize this document" }] }] }),
    });
    await r.text();
    console.log(`${response} -> HTTP ${r.status}`);
  } catch (e) {
    console.log(`${response} -> ${e}`);
  }
  await new Promise((ok) => setTimeout(ok, every * (0.6 + Math.random() * 0.8)));
}
