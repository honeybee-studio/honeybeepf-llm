<!-- translated from docs/faq.md at 250d516 -->

# Preguntas frecuentes { #faq }

!!! note ""
    Esta página es una traducción del original en inglés. Si difieren, el [original en inglés](https://honeybee-studio.github.io/honeybeepf-llm/faq/) es el correcto.

## ¿Hay que modificar las aplicaciones? { #do-i-have-to-change-my-apps }

No. Las aplicaciones siguen llamando directamente al proveedor con sus propios SDK y claves. No hay URL base que cambiar, ni biblioteca
que importar, ni nada que volver a desplegar. Consulte [Cómo funciona](how-it-works.md).

## ¿Se sitúa en la ruta de la petición o ralentiza las llamadas? { #does-it-sit-in-the-request-path-or-slow-my-calls-down }

No. El agente lee copias de los búferes TLS en el nodo; las peticiones van al proveedor exactamente igual que antes, y nada
espera al agente. La CPU y la memoria que usa, y el efecto sobre el rendimiento bajo carga, se miden en el
[benchmark de sobrecarga](benchmark.md).

## ¿Lee mis prompts y claves de API? { #does-it-read-my-prompts-and-api-keys }

Las sondas copian los bytes descifrados de cada llamada a la memoria del agente en el nodo, como tiene que hacerlo cualquier herramienta que lea tráfico TLS. El
agente los analiza en busca de los campos de uso y los descarta: los prompts, las respuestas, los encabezados y las claves de API nunca se exportan ni se
registran. La [revisión de seguridad](security-review.md#3-data-handling) indica qué se exporta y qué se registra, campo por campo.

## ¿Qué precisión tienen los números? { #how-accurate-are-the-numbers }

Los recuentos de tokens son los del propio proveedor: el agente lee los campos de uso de cada respuesta, incluidos los tokens de caché y de
razonamiento. El costo es una estimación: esos tokens multiplicados por una tabla de precios fechada, que se puede complementar con sus
precios de contrato ([Costo y precios](cost.md)). Las llamadas que el agente vio pero no pudo leer se cuentan, con un motivo,
en lugar de descartarse en silencio.

## ¿Qué proveedores, lenguajes y protocolos cubre? { #which-providers-languages-and-protocols-does-it-cover }

OpenAI, Anthropic, Gemini, Bedrock, Azure OpenAI, Vertex AI, Meta y muchas API compatibles con OpenAI, desde Python,
Node.js, Go, curl y cualquier otro programa que use OpenSSL, por HTTP/1.1 y HTTP/2 con streaming, y OpenAI Realtime,
Responses y Gemini Live por WebSocket. Todavía no: rustls, Java, otras API de WebSocket ni HTTP/3. La matriz completa está en
[Qué se admite](providers.md), y
[una métrica](guides/coverage.md) enumera las cargas de trabajo de su clúster que no se ven.

## Ya usamos un gateway de LLM como LiteLLM. ¿Necesitamos esto? { #we-already-run-an-llm-gateway-such-as-litellm-do-we-need-this }

Un gateway ve las llamadas que pasan por él; el agente ve todas las llamadas hechas desde el nodo, incluidas las que
esquivan el gateway, e indica el Pod que hizo cada una, sin cambios en las aplicaciones. Si activa la
[detección por forma](guides/detect-by-shape.md), un gateway dentro del clúster se registra dos veces (de la aplicación al gateway,
del gateway al proveedor); esa página explica cómo contarlo una sola vez.

## ¿Qué necesita en los nodos? { #what-does-it-need-on-the-nodes }

Linux 5.10 o posterior, o un kernel de RHEL 8.10, 9.6 o 9.7, con BTF, en x86-64 o arm64. El agente no es un contenedor privilegiado: se
ejecuta con `hostPID` y una lista corta de capacidades, explicadas en [Instalación](install.md#permissions-and-resources)
y en la [revisión de seguridad](security-review.md#2-privileges).

## ¿Funciona sin Kubernetes? { #does-it-work-without-kubernetes }

Sí, como binario en un host Linux, compilado sin la feature `k8s`; las métricas no llevan entonces etiquetas de espacio de nombres, Pod ni
carga de trabajo. Consulte [Ejecutar sin Kubernetes](run-binary.md).

## ¿Dónde puedo reportar un error o pedir un proveedor? { #where-do-i-report-a-bug-or-ask-for-a-provider }

En [GitHub Issues](https://github.com/honeybee-studio/honeybeepf-llm/issues).
