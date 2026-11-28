<!-- translated from docs/cost.md at 250d516 -->

# Costo y precios { #cost-and-prices }

!!! note ""
    Esta página es una traducción del original en inglés. Si difieren, el [original en inglés](https://honeybee-studio.github.io/honeybeepf-llm/cost/) es el correcto.

Cada llamada registrada recibe un costo estimado en USD: sus recuentos de tokens multiplicados por los precios del modelo.

## De dónde salen los precios { #where-prices-come-from }

- Una **tabla de precios integrada** y fechada, que se busca por el nombre del modelo de la respuesta. Un nombre con fecha como
  `claude-sonnet-4-5@20250929` corresponde a su modelo.
- **[Sus propios precios](guides/prices.md)**, superpuestos a la tabla: modelos que faltan en la tabla, o un descuento por contrato.
- Un modelo sin precio **nunca se registra como $0**: sus tokens van a `llm_unpriced_tokens_total` en su lugar.

**Cómo se elige un precio**

| | |
|---|---|
| Según | el nombre del modelo, no el host: Azure OpenAI, Bedrock, Vertex AI y los endpoints regionales se calculan con los precios de la API propia del proveedor, por lo que no se añade un recargo regional como la residencia de datos de OpenAI (+10%) |
| Precios | los precios de lista publicados por los proveedores; cada entrada de proveedor indica su página de precios y la fecha en que se comprobó |
| No incluido | tarifas de herramientas por petición, como la búsqueda web |

Compare el costo con `llm_unpriced_tokens_total` antes de confiar en un total.

!!! note
    Las métricas de costo solo aparecen si se cargó una tabla de precios. Si se rechaza la tabla integrada, solo se registra una advertencia y el agente
    sigue funcionando sin ellas.

## Gasto por carga de trabajo { #spend-per-workload }

```promql
sum by (target_namespace, target_workload_name) (rate(honeybeepf_llm_llm_cost_usd_total[5m])) * 60
```

USD por minuto, por espacio de nombres y carga de trabajo.

## Llamadas a través de un gateway { #calls-through-a-gateway }

Una llamada que pasa por un gateway dentro del clúster (LiteLLM, un proxy de la empresa) se hace dos veces: de la aplicación al
gateway, y del gateway al proveedor. Cuando el agente registra ambas, declare el gateway como
[proveedor](guides/custom-providers.md) con `gateway: true`, para poder distinguir los dos registros:

| Registro | Etiquetas | Úselo para |
|---|---|---|
| aplicación → gateway | el Pod de la aplicación, `hop="gateway"` | gasto por equipo o carga de trabajo |
| gateway → proveedor | el Pod del gateway, sin `hop` | gasto total: el modelo y el uso propios del proveedor |

Las series de cada carga de trabajo solo contienen sus propias llamadas, así que el gasto por espacio de nombres o carga de trabajo no necesita filtro, y el gateway aparece
como una carga de trabajo más. Para sumar todas las cargas de trabajo contando cada llamada una sola vez, excluya el salto por el gateway:

```promql
sum(increase(honeybeepf_llm_llm_cost_usd_total{hop!="gateway"}[1d]))
```

El dashboard de costos incluido hace esto en sus totales y en sus paneles por modelo, por modalidad y por operación.

!!! warning
    Defina `gateway: true` solo para un gateway cuyas llamadas a los proveedores registre este agente: uno que se ejecute en los
    nodos del clúster y llame a los proveedores por TLS o mediante una entrada `plaintext`. Si el gateway está fuera del clúster, su
    registro es el único, y el filtro anterior lo descartaría.

## Las métricas de costo { #the-cost-metrics }

| Métrica | Qué cuenta | Etiquetas adicionales |
|---|---|---|
| `honeybeepf_llm_llm_cost_usd_total` | el costo de cada llamada, en USD | `cost_type`, `price_table_version` |
| `honeybeepf_llm_llm_cache_savings_ceiling_usd_total` | cuánto podría haber ahorrado como máximo el almacenamiento en caché de prompts | `cache_price`, `price_table_version` |
| `honeybeepf_llm_llm_unpriced_tokens_total` | tokens de las llamadas a las que no se pudo asignar precio | `reason`, `price_table_version` |

Las tres llevan las etiquetas por llamada (`model`, `status`, `attempt`, `target_namespace`, `target_pod_name`,
`target_workload_kind`, `target_workload_name`), pero no `transport` ni `operation`. Los contadores en USD no llevan unidad, así que
el exportador no añade ninguna al nombre.

### `cost_type`: cómo se desglosa el costo { #cost_type-how-the-cost-is-split }

Como en la factura de un proveedor. Una parte que vale cero no se añade, así que una llamada solo tiene las series que usó.

| `cost_type` | Se factura por |
|---|---|
| `input` | tokens de prompt que ni se leyeron de la caché ni se escribieron en ella |
| `cache_read` | tokens de prompt servidos desde la caché del proveedor |
| `cache_write` | tokens de prompt escritos en la caché del proveedor |
| `output` | todos los tokens de completion, incluidos los de razonamiento |

### `attempt`: servida o rechazada { #attempt-served-or-declined }

| `attempt` | La llamada |
|---|---|
| `served` | el proveedor la sirvió |
| `declined` | un intento anterior de fallback del lado del servidor que el proveedor facturó pero redirigió a otro destino: solo tokens y costo, sin recuento de peticiones ni latencia |

Las llamadas registradas como error no generan costo.

### `price_table_version` { #price_table_version }

El campo `version` de la tabla integrada, con `+custom-<hash>` añadido cuando sus precios se combinan con ella. Un cambio de precio
aparece como una serie nueva.

### Techo de ahorro por caché { #cache-savings-ceiling }

Los tokens de entrada no cacheados de la llamada, calculados al precio de lectura de caché del modelo en lugar de a su precio de entrada: un límite superior de
lo que podría ahorrar el almacenamiento en caché de prompts. Solo se emite para llamadas con precio que no son de embeddings, y solo cuando es mayor que cero.

| `cache_price` | Significado |
|---|---|
| `known` | el modelo tiene un precio de lectura de caché |
| `assumed_free` | no lo tiene, así que el límite es el costo de entrada completo |

### Tokens sin precio { #unpriced-tokens }

| `reason` | No se pudo asignar precio a la llamada porque |
|---|---|
| `unknown_model` | faltaba el nombre del modelo o no estaba en la tabla |
| `missing_price` | falta un precio que necesita la llamada o ya venció su vigencia |
| `unsupported_modifier` | el nivel de servicio, la velocidad o el modificador geográfico de la llamada no tiene precio |
| `inconsistent_usage` | los recuentos de tokens de caché informados superan el total de prompt del que forman parte |
