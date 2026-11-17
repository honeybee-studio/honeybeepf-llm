# Add a provider

The built-in list covers the major providers and many OpenAI-compatible ones ([What is supported](../providers.md)). To
record another API, for example a self-hosted Ollama or a company gateway, describe it in the chart values by host,
path and the fields of its usage.

```yaml
builtinProbes:
  llm:
    providers:
      - name: "ollama"
        hosts: ["ollama.internal"]           # substring match; one "*" must cover the whole host ("llm.*.internal")
        paths: ["/api/generate", "/api/chat"] # substring match; a trailing "$" anchors the end
        response:
          usage_path: "usage"                # JSON path to the usage object
          prompt_tokens: "prompt_eval_count"  # field for input tokens
          completion_tokens: "eval_count"     # field for output tokens
          model_path: "model"
        request_extractor: "messages"        # how to read the prompt: messages, contents, prompt or input
```

An OpenAI-compatible API needs no `response` block. If the server is reached over plain HTTP inside the cluster, add
`plaintext` as well; see [Self-hosted models over plain HTTP](plaintext.md). To cost its calls, add the model's
[prices](prices.md).

## How entries combine

`builtinProbes.llm.providers` in `values.yaml` describes providers by host, path and usage fields, in the format of
the [built-in list](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/honeybeepf-llm/honeybeepf-llm-core/src/providers/default_providers.json). They are added to the
built-in providers; a provider with a built-in name (`openai`, `anthropic`, `gemini`, `bedrock`, matched exactly)
replaces every built-in entry of that name, so an `openai` entry must also cover the Responses API and an
`anthropic` entry Claude on Vertex AI. Usage is read by matching the response's fields, not its host, with the
built-in entries tried first: a gateway that returns an OpenAI-, Anthropic- or Gemini-shaped body is read with the
built-in fields.
