<!-- translated from docs/faq.md at 250d516 -->

# Perguntas frequentes { #faq }

!!! note ""
    Esta página é uma tradução do original em inglês. Se houver diferença, vale o [original em inglês](https://honeybee-studio.github.io/honeybeepf-llm/faq/).

## Preciso mudar minhas aplicações? { #do-i-have-to-change-my-apps }

Não. As aplicações continuam chamando o provedor diretamente, com os próprios SDKs e chaves. Não há URL base para
mudar, biblioteca para importar nem deploy para refazer. Veja [Como funciona](how-it-works.md).

## Ele fica no caminho da requisição ou deixa minhas chamadas mais lentas? { #does-it-sit-in-the-request-path-or-slow-my-calls-down }

Não. O agente lê cópias dos buffers TLS no nó; as requisições vão para o provedor exatamente como antes, e nada espera
pelo agente. A CPU e a memória que ele usa, e o efeito no throughput sob carga, foram medidos no
[benchmark de overhead](benchmark.md).

## Ele lê meus prompts e chaves de API? { #does-it-read-my-prompts-and-api-keys }

As probes copiam os bytes descriptografados de cada chamada para a memória do agente no nó, como qualquer ferramenta
que lê TLS precisa fazer. O agente os interpreta em busca dos campos de uso e os descarta: prompts, respostas,
cabeçalhos e chaves de API nunca são exportados nem vão para o log. A [revisão de segurança](security-review.md#3-data-handling)
diz o que é exportado e registrado no log, campo por campo.

## Qual a precisão dos números? { #how-accurate-are-the-numbers }

As contagens de tokens são as do próprio provedor: o agente lê os campos de uso de cada resposta, incluindo tokens de
cache e de raciocínio. O custo é uma estimativa: esses tokens multiplicados por uma tabela de preços datada, sobre a
qual você pode aplicar seus preços de contrato ([Custos e preços](cost.md)). Chamadas que o agente viu, mas não
conseguiu ler, são contadas, com um motivo, em vez de descartadas em silêncio.

## Quais provedores, linguagens e protocolos ele cobre? { #which-providers-languages-and-protocols-does-it-cover }

OpenAI, Anthropic, Gemini, Bedrock, Azure OpenAI, Vertex AI, Meta e muitas APIs compatíveis com a OpenAI, a partir de
Python, Node.js, Go, curl e qualquer outra coisa que use OpenSSL, via HTTP/1.1 e HTTP/2 com streaming, e OpenAI
Realtime, Responses e Gemini Live via WebSocket. Ainda não: rustls, Java, outras APIs WebSocket e HTTP/3. A matriz
completa está em [O que é suportado](providers.md), e
[uma métrica](guides/coverage.md) lista as cargas de trabalho do seu cluster que o agente não vê.

## Já usamos um gateway de LLM como o LiteLLM. Precisamos disto? { #we-already-run-an-llm-gateway-such-as-litellm-do-we-need-this }

Um gateway vê as chamadas roteadas por ele; o agente vê todas as chamadas feitas a partir do nó, incluindo as que
contornam o gateway, e indica o Pod que fez cada uma, sem nenhuma mudança nas aplicações. Se você ativar a
[detecção pelo formato](guides/detect-by-shape.md), um gateway dentro do cluster é registrado duas vezes (aplicação até
o gateway, gateway até o provedor); a página mostra como contá-lo uma vez só.

## Do que ele precisa nos nós? { #what-does-it-need-on-the-nodes }

Linux 5.10 ou posterior, ou um kernel RHEL 8.10, 9.6 ou 9.7, com BTF, em x86-64 ou arm64. O agente não é privilegiado:
ele roda com `hostPID` e uma lista curta de capabilities, explicada em [Instalação](install.md#permissions-and-resources)
e na [revisão de segurança](security-review.md#2-privileges).

## Funciona sem Kubernetes? { #does-it-work-without-kubernetes }

Sim, como um binário em um host Linux, compilado sem a feature `k8s`; nesse caso as métricas não têm rótulos de
namespace, Pod ou carga de trabalho. Veja [Executar sem Kubernetes](run-binary.md).

## Onde relato um bug ou peço suporte a um provedor? { #where-do-i-report-a-bug-or-ask-for-a-provider }

Nas [GitHub Issues](https://github.com/honeybee-studio/honeybeepf-llm/issues).
