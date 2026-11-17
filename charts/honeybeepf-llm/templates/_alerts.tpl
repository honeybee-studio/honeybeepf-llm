{{/*
Prometheus rule groups for LLM spend and capture health, as a YAML list. Rendered into a PrometheusRule
(templates/prometheusrule.yaml) and copied into the bundled Prometheus chart; ci/test-alerts.sh tests both.
Spend and call rules group by target_namespace, the calling pod's namespace: `namespace` is the collector's.
*/}}
{{- define "honeybeepf-llm.alertGroups" -}}
{{- $a := .Values.alerts -}}
{{- $cost := "honeybeepf_llm_llm_cost_usd_total" -}}
{{- $reqs := "honeybeepf_llm_llm_requests_total" -}}
{{- $dropped := "honeybeepf_llm_llm_responses_unparsed_total{reason!~\"agent_session|usage_absent\"}" -}}
{{- if or $a.budgets (gt (float64 $a.defaultDailyBudgetUSD) 0.0) }}
- name: honeybeepf-llm.budgets
  rules:
  # One rule, so the budget series has a single source; promtool flags a name recorded twice.
  - record: honeybeepf_llm:namespace_daily_budget_usd
    expr: |
      {{- $terms := list }}
      {{- range $ns, $usd := $a.budgets }}
      {{- $terms = append $terms (printf "label_replace(vector(%v), \"target_namespace\", %q, \"\", \"\")" $usd $ns) }}
      {{- end }}
      {{- if gt (float64 $a.defaultDailyBudgetUSD) 0.0 }}
      {{- $terms = append $terms (printf "sum by (target_namespace) (increase(%s{target_namespace!~%q}[24h])) * 0 + %v" $cost (keys $a.budgets | sortAlpha | join "|") $a.defaultDailyBudgetUSD) }}
      {{- end }}
      {{ join "\n      or " $terms }}
{{- end }}
- name: honeybeepf-llm.alerts
  rules:
  - alert: LLMSpendOverBudget
    expr: sum by (target_namespace) (increase({{ $cost }}[24h])) > on (target_namespace) honeybeepf_llm:namespace_daily_budget_usd
    for: 5m
    labels:
      severity: warning
    annotations:
      summary: {{`"LLM spend in {{ $labels.target_namespace }} is over its daily budget"`}}
      description: {{`"{{ $labels.target_namespace }} spent ${{ $value | printf \"%.2f\" }} on LLM APIs in the last 24 hours, more than its budget."`}}
  - alert: LLMSpendPaceOverBudget
    expr: sum by (target_namespace) (rate({{ $cost }}[1h])) * 86400 > on (target_namespace) honeybeepf_llm:namespace_daily_budget_usd
    for: 15m
    labels:
      severity: warning
    annotations:
      summary: {{`"LLM spend in {{ $labels.target_namespace }} is on pace to pass its daily budget"`}}
      description: {{`"At the last hour's rate {{ $labels.target_namespace }} would spend ${{ $value | printf \"%.2f\" }} on LLM APIs in a day, more than its budget."`}}
  - alert: LLMSpendSpike
    expr: |
      sum by (target_namespace) (increase({{ $cost }}[1h]))
        > {{ $a.spike.factor }} * sum by (target_namespace) (increase({{ $cost }}[1h] offset 1d))
      and
      sum by (target_namespace) (increase({{ $cost }}[1h])) > {{ $a.spike.minUSD }}
    for: 15m
    labels:
      severity: info
    annotations:
      summary: {{ printf "LLM spend in {{ $labels.target_namespace }} is over %v times the same hour yesterday" $a.spike.factor | quote }}
      description: {{`"{{ $labels.target_namespace }} spent ${{ $value | printf \"%.2f\" }} on LLM APIs in the last hour."`}}
  - alert: LLMCallsWentSilent
    expr: |
      sum by (target_namespace) (increase({{ $reqs }}[1h] offset 1d)) >= {{ $a.silentMinCalls }}
      unless on (target_namespace)
      sum by (target_namespace) (increase({{ $reqs }}[1h])) > 0
    for: 30m
    labels:
      severity: warning
    annotations:
      summary: {{`"No LLM calls recorded in {{ $labels.target_namespace }} for an hour"`}}
      description: {{`"{{ $labels.target_namespace }} made {{ $value | printf \"%.0f\" }} LLM calls in this hour yesterday and none in the last hour. Either its traffic stopped or the calls are no longer being recorded."`}}
  - alert: LLMCallsNotRecorded
    expr: |
      sum by (target_namespace) (increase({{ $dropped }}[15m]))
        / (
          sum by (target_namespace) (increase({{ $dropped }}[15m]))
          + (sum by (target_namespace) (increase({{ $reqs }}[15m])) or sum by (target_namespace) (increase({{ $dropped }}[15m])) * 0)
        )
        > {{ $a.notRecordedRatio }}
      and
      sum by (target_namespace) (increase({{ $dropped }}[15m])) >= 5
    for: 15m
    labels:
      severity: warning
    annotations:
      summary: {{`"LLM calls in {{ $labels.target_namespace }} are being seen but not recorded"`}}
      description: {{`"{{ $value | humanizePercentage }} of the LLM calls seen from {{ $labels.target_namespace }} in the last 15 minutes could not be recorded; llm_responses_unparsed_total has the reasons."`}}
  - alert: LLMTokensUnpriced
    expr: sum by (target_namespace, reason) (increase(honeybeepf_llm_llm_unpriced_tokens_total[1h])) > 0
    for: 30m
    labels:
      severity: info
    annotations:
      summary: {{`"LLM tokens in {{ $labels.target_namespace }} have no price ({{ $labels.reason }})"`}}
      description: {{`"{{ $value | humanize }} tokens in the last hour were recorded without a price, so cost is under-reported. Add the model to a price file."`}}
  - alert: LLMCaptureEventsDropped
    expr: sum by (k8s_node_name) (rate(honeybeepf_llm_llm_events_total{stage="reserve_fail"}[5m])) > 0
    for: 10m
    labels:
      severity: warning
    annotations:
      summary: {{`"The agent on {{ $labels.k8s_node_name }} is dropping captured TLS data"`}}
      description: {{`"The ring buffer on {{ $labels.k8s_node_name }} was full {{ $value | humanize }} times per second, so some LLM calls there were lost."`}}
  - alert: LLMWorkloadNotHooked
    expr: sum by (target_namespace, target_workload_name) (honeybeepf_llm_llm_tls_coverage_pods{tls="openssl_not_hooked"}) > 0
    for: 30m
    labels:
      severity: warning
    annotations:
      summary: {{`"{{ $labels.target_namespace }}/{{ $labels.target_workload_name }} uses OpenSSL the agent could not hook"`}}
      description: {{`"{{ $value }} pods of this workload map a libssl the probes are not attached to, so their LLM calls are not recorded. The agent log has the attach error."`}}
  - alert: HoneybeepfAgentMissing
    expr: |
      count by (k8s_node_name) (honeybeepf_llm_active_probes offset 1h)
      unless on (k8s_node_name)
      count by (k8s_node_name) (honeybeepf_llm_active_probes)
    for: 10m
    labels:
      severity: critical
    annotations:
      summary: {{`"The honeybeepf-llm agent on {{ $labels.k8s_node_name }} stopped reporting"`}}
      description: {{`"The agent on {{ $labels.k8s_node_name }} reported an hour ago and not now, so LLM calls on that node are not recorded. This also fires for about an hour after a node is removed."`}}
{{- end }}
