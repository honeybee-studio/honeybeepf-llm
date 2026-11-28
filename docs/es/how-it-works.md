<!-- translated from docs/en/how-it-works.md, git blob 4df014123bcb -->

# Cómo funciona { #how-it-works }

!!! note ""
    Esta página es una traducción del original en inglés. Si difieren, el [original en inglés](https://honeybee-studio.github.io/honeybeepf-llm/how-it-works/) es el correcto.

Una aplicación que llama a un proveedor de LLM entrega la petición a su biblioteca TLS en texto plano y recibe de ella la respuesta
en texto plano. honeybeepf-llm lee una copia de esos bytes en ese punto, en el nodo, con eBPF. La aplicación, su SDK y la
ruta de red hasta el proveedor no cambian.

![Las uprobes en SSL_write/SSL_read copian el texto plano a un búfer circular; el agente analiza HTTP, identifica el proveedor, asocia el cgroup a un Pod y exporta métricas OTLP](../images/how-it-works.svg)

## 1. Encontrar el código TLS { #1-find-the-tls-code }

El agente se ejecuta como DaemonSet, un Pod por nodo. Examina cada proceso de su nodo y localiza dónde hace
el cifrado TLS:

| El proceso | El agente se conecta a |
|---|---|
| mapea una `libssl.so` compartida (Python, Node.js de los paquetes de la distribución, curl) | `SSL_read`, `SSL_write` y sus variantes `_ex` en esa biblioteca |
| tiene OpenSSL integrado en su ejecutable (los binarios oficiales de Node.js) | las mismas funciones en el ejecutable |
| es un ejecutable de Go 1.20+ | `crypto/tls` `(*Conn).Write`, `Read` y cada retorno de `Read`, localizados a partir de la tabla de funciones de Go, incluidos los binarios sin símbolos |

También se conecta a los procesos que se inician más tarde: una sonda en `exec` y en la apertura de un archivo `libssl` le avisa de los
nuevos. Los programas que usan otra pila TLS, como rustls o JSSE de Java, aún no se ven; la
[métrica de cobertura](guides/coverage.md) los enumera.

## 2. Copiar el texto plano { #2-copy-the-plaintext }

Cada sonda es un programa eBPF que se ejecuta cuando se llama a la función o cuando esta retorna. Copia el búfer de texto plano (hasta
16 KB de una lectura y 4 KB de una escritura), el id del proceso y el id del cgroup a un búfer circular compartido con el agente. Los
programas solo leen: ninguno modifica la llamada, los datos ni la conexión.

Un servidor de modelos dentro del clúster al que se llama por HTTP sin cifrar, como Ollama o vLLM, no tiene biblioteca TLS que interceptar. Para un proveedor
marcado con `plaintext`, el agente copia en su lugar los bytes en el socket (consulte
[Modelos autoalojados por HTTP sin cifrar](guides/plaintext.md)).

## 3. Convertir los bytes en llamadas { #3-turn-bytes-into-calls }

En el espacio de usuario, el agente reensambla los bytes de cada conexión en peticiones y respuestas HTTP/1.1 o HTTP/2, o en mensajes
WebSocket (Realtime, Responses, Gemini Live),
los descomprime (`gzip`, `br`, `zstd`, `deflate`) y sigue los flujos de eventos enviados por el servidor hasta su final. Determina el
proveedor a partir del host y la ruta de la petición y lee los recuentos de tokens de los campos de uso de la respuesta: tokens de prompt,
de completion, de lectura y escritura de caché, y de razonamiento.

Una llamada que el agente vio pero no pudo leer no se descarta en silencio: se cuenta en `llm_responses_unparsed_total` con
el motivo.

## 4. Encontrar el Pod { #4-find-the-pod }

El cgroup del proceso indica el ID de su contenedor. Un watch sobre los Pods de este nodo asocia el contenedor a su espacio de nombres, su Pod y
la carga de trabajo a la que pertenece, como un Deployment o un CronJob.

## 5. Calcular el precio de la llamada { #5-price-the-call }

El nombre del modelo de la respuesta se busca en una tabla de precios integrada y fechada, a la que se superponen sus propios precios o descuentos.
El costo de la llamada se desglosa como lo facturan los proveedores: entrada, lectura de caché, escritura de caché y salida. Un modelo sin precio
se cuenta en `llm_unpriced_tokens_total` y nunca se registra como $0.

## 6. Exportar { #6-export }

El agente exporta tokens, peticiones, latencia y costo como métricas de OpenTelemetry por OTLP, etiquetadas por espacio de nombres, Pod,
carga de trabajo y modelo, y registra en el log una línea por llamada con su modelo, Pod, latencia y recuentos de tokens (y la ruta de la petición, en el nivel `debug`). Los
bytes capturados permanecen en la memoria del agente y se descartan una vez analizados: los prompts, las respuestas, los encabezados y las claves de API
nunca se exportan ni se registran.

La [revisión de seguridad](security-review.md) enumera cada programa eBPF, los privilegios que necesita el agente y lo que puede y
no puede ver.
