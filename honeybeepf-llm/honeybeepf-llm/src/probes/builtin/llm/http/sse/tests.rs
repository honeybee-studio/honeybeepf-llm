use super::super::test_support::{GEMINI_ZERO_USAGE, RESPONSES_CREATED, tokens};
use super::super::{Http2Parser, Http11Parser, ProtocolParser};
use super::*;

const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":15}}\n\n",
);

// Gemini Interactions streaming: usage rides inside `interaction`, cumulative, last event wins.
const INTERACTIONS_SSE: &str = concat!(
    "event: interaction.created\n",
    "data: {\"event_id\":\"e1\",\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"int_1\",",
    "\"model\":\"gemini-3.6-flash\",\"status\":\"in_progress\",",
    "\"usage\":{\"total_input_tokens\":7,\"total_output_tokens\":0,\"total_thought_tokens\":0,\"total_cached_tokens\":0,\"total_tokens\":7}}}\n\n",
    "event: interaction.completed\n",
    "data: {\"event_id\":\"e2\",\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"int_1\",",
    "\"model\":\"gemini-3.6-flash\",\"status\":\"completed\",",
    "\"usage\":{\"total_input_tokens\":7,\"total_output_tokens\":23,\"total_thought_tokens\":49,\"total_cached_tokens\":0,\"total_tokens\":79}}}\n\n",
);

const DONE_IN_TEXT: &str = "The stream ends with data: [DONE]";

const ANTHROPIC_SSE_CACHE_START: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"type\":\"message\",\"model\":\"claude-test\",",
    "\"usage\":{\"input_tokens\":50,\"cache_creation_input_tokens\":2000,\"cache_read_input_tokens\":100000,\"output_tokens\":1}}}\n\n",
);

fn http11_sse(body: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{body}").into_bytes()
}

fn cache(info: &UsageInfo) -> (Option<u64>, Option<u64>) {
    (info.cache_read_tokens, info.cache_write_tokens)
}

#[test]
fn anthropic_sse_merges_message_start_and_delta_usage() {
    let body =
        format!("{ANTHROPIC_SSE}event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n");
    let info = Http11Parser
        .parse_response(&http11_sse(&body))
        .expect("usage");
    assert_eq!((info.prompt_tokens, info.completion_tokens), (25, 15));
    assert_eq!(info.model.as_deref(), Some("claude-test"));
}

#[test]
fn anthropic_sse_delta_with_both_counts() {
    let body = concat!(
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":25,\"output_tokens\":15}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let info = Http11Parser.parse_response(&http11_sse(body));
    assert_eq!(tokens(info), Some((25, 15)));
}

#[test]
fn anthropic_mid_output_fallback_sse_reads_served_model_from_the_iteration() {
    // Mid-output decline: message_start named the REQUESTED (declined) model, so the served
    // model must come from the fallback_message entry, not from the root.
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-fable-5\",",
        "\"usage\":{\"input_tokens\":412,\"output_tokens\":0}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"part\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":412,\"output_tokens\":264,",
        "\"iterations\":[{\"type\":\"message\",\"model\":\"claude-fable-5\",\"input_tokens\":535,\"output_tokens\":40},",
        "{\"type\":\"fallback_message\",\"model\":\"claude-opus-4-8\",\"input_tokens\":412,\"output_tokens\":264}]}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let info = Http11Parser
        .parse_response(&http11_sse(body))
        .expect("usage");
    assert_eq!(info.model.as_deref(), Some("claude-opus-4-8"));
    assert_eq!((info.prompt_tokens, info.completion_tokens), (412, 264));
    assert_eq!(info.iterations.len(), 1);
    assert_eq!(info.iterations[0].model.as_deref(), Some("claude-fable-5"));
}

#[test]
fn anthropic_fallback_sse_reads_category_from_content_block_start() {
    // Declined before output: the refusal category rides a content_block_start fallback block,
    // which the cursor must capture so a billed category (here `bio`) keeps its cost.
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-fable-5\",\"content\":[],",
        "\"usage\":{\"input_tokens\":412,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"fallback\",",
        "\"from\":{\"model\":\"claude-fable-5\"},\"to\":{\"model\":\"claude-opus-4-8\"},",
        "\"trigger\":{\"type\":\"refusal\",\"category\":\"bio\"}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":412,\"output_tokens\":264,",
        "\"iterations\":[{\"type\":\"message\",\"model\":\"claude-fable-5\",\"input_tokens\":535,\"output_tokens\":0},",
        "{\"type\":\"fallback_message\",\"model\":\"claude-opus-4-8\",\"input_tokens\":412,\"output_tokens\":264}]}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let info = Http11Parser
        .parse_response(&http11_sse(body))
        .expect("usage");
    assert_eq!(info.model.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(info.iterations.len(), 1);
    let declined = &info.iterations[0];
    assert_eq!(declined.model.as_deref(), Some("claude-fable-5"));
    assert_eq!(declined.completion_tokens, 0);
    assert!(!declined.cost_suppressed);
}

#[test]
fn anthropic_fallback_sse_suppresses_a_non_billed_before_output_decline() {
    // Same streaming shape, but `cyber` is not billed: the before-output decline keeps its
    // tokens with cost suppressed, proving the category reaches the rule through the stream.
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-fable-5\",\"content\":[],",
        "\"usage\":{\"input_tokens\":412,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"fallback\",",
        "\"from\":{\"model\":\"claude-fable-5\"},\"to\":{\"model\":\"claude-opus-4-8\"},",
        "\"trigger\":{\"type\":\"refusal\",\"category\":\"cyber\"}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":412,\"output_tokens\":264,",
        "\"iterations\":[{\"type\":\"message\",\"model\":\"claude-fable-5\",\"input_tokens\":535,\"output_tokens\":0},",
        "{\"type\":\"fallback_message\",\"model\":\"claude-opus-4-8\",\"input_tokens\":412,\"output_tokens\":264}]}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let info = Http11Parser
        .parse_response(&http11_sse(body))
        .expect("usage");
    assert_eq!(info.iterations.len(), 1);
    assert!(info.iterations[0].cost_suppressed);
}

#[test]
fn gemini_interactions_sse_ends_on_completed_and_sums_output_and_thoughts() {
    let info = Http11Parser
        .parse_response(&http11_sse(INTERACTIONS_SSE))
        .expect("usage");
    // Output excludes thoughts (7 + 23 + 49 = 79 = total), so completion is 23 + 49.
    assert_eq!((info.prompt_tokens, info.completion_tokens), (7, 72));
    assert_eq!(info.thoughts_tokens, Some(49));
    assert_eq!(info.model.as_deref(), Some("gemini-3.6-flash"));
}

#[test]
fn gemini_interactions_sse_created_usage_alone_does_not_end_the_stream() {
    // Only the `interaction.created` event (cumulative usage, in_progress): no terminal marker yet.
    let created = INTERACTIONS_SSE
        .split("event: interaction.completed")
        .next()
        .expect("split yields a first element");
    assert_eq!(
        tokens(Http11Parser.parse_response(&http11_sse(created))),
        None
    );
}

#[test]
fn gemini_interactions_sse_null_usage_event_does_not_erase_earlier_usage() {
    // A status_update carrying `usage: null` must not be treated as a usage event: otherwise its
    // null would overwrite the accumulated usage, and a later completed event would still have to
    // win cleanly.
    let body = concat!(
        "event: interaction.created\n",
        "data: {\"event_id\":\"e1\",\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"int_1\",",
        "\"model\":\"gemini-3.6-flash\",\"status\":\"in_progress\",",
        "\"usage\":{\"total_input_tokens\":7,\"total_output_tokens\":0,\"total_thought_tokens\":0,\"total_cached_tokens\":0,\"total_tokens\":7}}}\n\n",
        "event: interaction.status_update\n",
        "data: {\"event_id\":\"e2\",\"event_type\":\"interaction.status_update\",\"interaction\":{\"id\":\"int_1\",\"status\":\"in_progress\",\"usage\":null}}\n\n",
        "event: interaction.completed\n",
        "data: {\"event_id\":\"e3\",\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"int_1\",\"status\":\"completed\",",
        "\"usage\":{\"total_input_tokens\":7,\"total_output_tokens\":23,\"total_thought_tokens\":49,\"total_cached_tokens\":0,\"total_tokens\":79}}}\n\n",
    );
    let info = Http11Parser
        .parse_response(&http11_sse(body))
        .expect("usage");
    assert_eq!((info.prompt_tokens, info.completion_tokens), (7, 72));
}

#[test]
fn gemini_interactions_sse_model_text_naming_the_marker_does_not_end_early() {
    // A step event whose model text mentions interaction.completed unquoted must not end the
    // stream; only the quoted event_type token does.
    let body = concat!(
        "event: interaction.status_update\n",
        "data: {\"event_id\":\"e1\",\"event_type\":\"interaction.status_update\",\"interaction\":{\"id\":\"int_1\",",
        "\"status\":\"in_progress\",\"step\":{\"text\":\"I will send interaction.completed to finish\"}}}\n\n",
        "event: interaction.completed\n",
        "data: {\"event_id\":\"e2\",\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"int_1\",",
        "\"model\":\"gemini-3.6-flash\",\"status\":\"completed\",",
        "\"usage\":{\"total_input_tokens\":7,\"total_output_tokens\":23,\"total_thought_tokens\":49,\"total_cached_tokens\":0,\"total_tokens\":79}}}\n\n",
    );
    let info = Http11Parser
        .parse_response(&http11_sse(body))
        .expect("usage");
    assert_eq!((info.prompt_tokens, info.completion_tokens), (7, 72));
}

#[test]
fn gemini_interactions_sse_error_event_leaves_the_call_unrecorded() {
    // The ErrorEvent (`event: error`, verbatim from the docs) aborts the call with no usage.
    let body = concat!(
        "event: error\n",
        "data: {\"error\":{\"message\":\"Deadline expired before operation could complete.\",",
        "\"code\":\"gateway_timeout\"},\"event_type\":\"error\"}\n\n",
    );
    assert_eq!(tokens(Http11Parser.parse_response(&http11_sse(body))), None);
}

#[test]
fn gemini_interactions_sse_error_event_poisons_a_later_completed() {
    // Partial usage arrived, then an error aborted the stream; a later completed event must not
    // resurrect the call, since once the stream errored its usage is no longer trusted.
    let body = concat!(
        "event: interaction.created\n",
        "data: {\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"int_1\",\"model\":\"gemini-3.6-flash\",",
        "\"status\":\"in_progress\",\"usage\":{\"total_input_tokens\":7,\"total_output_tokens\":0,\"total_tokens\":7}}}\n\n",
        "event: error\n",
        "data: {\"error\":{\"message\":\"boom\",\"code\":\"internal\"},\"event_type\":\"error\"}\n\n",
        "event: interaction.completed\n",
        "data: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"int_1\",\"model\":\"gemini-3.6-flash\",",
        "\"status\":\"completed\",\"usage\":{\"total_input_tokens\":7,\"total_output_tokens\":23,\"total_thought_tokens\":49,\"total_tokens\":79}}}\n\n",
    );
    assert_eq!(tokens(Http11Parser.parse_response(&http11_sse(body))), None);
}

#[test]
fn gemini_interactions_sse_failed_completion_without_usage_records_zero() {
    // A failed interaction ends on interaction.completed with no usage object; the call is still
    // finished, recorded as zero tokens — the same as a failed Responses stream.
    let body = concat!(
        "event: interaction.completed\n",
        "data: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"int_1\",",
        "\"model\":\"gemini-3.6-flash\",\"status\":\"failed\"}}\n\n",
    );
    assert_eq!(
        tokens(Http11Parser.parse_response(&http11_sse(body))),
        Some((0, 0))
    );
}

#[test]
fn gemini_interactions_sse_cancelled_or_incomplete_follows_its_usage() {
    // A cancelled or incomplete interaction is recorded from the usage it carries (a mid-stream
    // cancel keeps the tokens so far) and left unrecorded only when it has none at all, as
    // Responses treats its own incomplete and cancel rather than fabricating a zero-token call.
    for status in ["cancelled", "incomplete"] {
        let with_usage = format!(
            "event: interaction.completed\n\
             data: {{\"event_type\":\"interaction.completed\",\"interaction\":{{\"id\":\"int_1\",\
             \"model\":\"gemini-3.6-flash\",\"status\":\"{status}\",\
             \"usage\":{{\"total_input_tokens\":7,\"total_output_tokens\":5,\"total_tokens\":12}}}}}}\n\n"
        );
        assert_eq!(
            tokens(Http11Parser.parse_response(&http11_sse(&with_usage))),
            Some((7, 5)),
            "{status} with usage"
        );

        let no_usage = format!(
            "event: interaction.completed\n\
             data: {{\"event_type\":\"interaction.completed\",\"interaction\":{{\"id\":\"int_1\",\
             \"model\":\"gemini-3.6-flash\",\"status\":\"{status}\"}}}}\n\n"
        );
        assert_eq!(
            tokens(Http11Parser.parse_response(&http11_sse(&no_usage))),
            None,
            "{status} without usage"
        );
    }
}

#[test]
fn anthropic_sse_text_quoting_done_waits_for_message_stop() {
    let start = format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m1\",\"model\":\"claude-test\",\"usage\":{{\"input_tokens\":25,\"output_tokens\":1}}}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{DONE_IN_TEXT}\"}}}}\n\n"
    );
    let info = Http11Parser.parse_response(&http11_sse(&start));
    assert_eq!(tokens(info), None);

    let full = format!(
        "{start}event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":15}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    );
    let info = Http11Parser.parse_response(&http11_sse(&full));
    assert_eq!(tokens(info), Some((25, 15)));
}

#[test]
fn gemini_sse_text_quoting_done_is_incomplete() {
    let body = format!(
        "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{DONE_IN_TEXT}\"}}]}}}}],\
         \"usageMetadata\":{{\"promptTokenCount\":7,\"candidatesTokenCount\":2}},\"modelVersion\":\"gemini-test\"}}\r\n\r\n"
    );
    let info = Http11Parser.parse_response(&http11_sse(&body));
    assert_eq!(tokens(info), None);
}

#[test]
fn anthropic_sse_without_message_stop_is_incomplete() {
    let info = Http11Parser.parse_response(&http11_sse(ANTHROPIC_SSE));
    assert_eq!(tokens(info), None);
}

#[test]
fn anthropic_sse_input_total_includes_cache() {
    let body = format!(
        "{ANTHROPIC_SSE_CACHE_START}event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":300}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    );
    let info = Http11Parser.parse_response(&http11_sse(&body));
    assert_eq!(tokens(info), Some((102_050, 300)));
}

#[test]
fn anthropic_sse_cumulative_delta_overrides_message_start_cache() {
    let body = format!(
        "{ANTHROPIC_SSE_CACHE_START}event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"input_tokens\":60,\
         \"cache_creation_input_tokens\":2000,\"cache_read_input_tokens\":100500,\"output_tokens\":300}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    );
    let info = Http11Parser
        .parse_response(&http11_sse(&body))
        .expect("usage");
    assert_eq!((info.prompt_tokens, info.completion_tokens), (102_560, 300));
    assert_eq!(cache(&info), (Some(100_500), Some(2000)));
}

#[test]
fn anthropic_sse_null_delta_counts_keep_message_start_cache() {
    let body = format!(
        "{ANTHROPIC_SSE_CACHE_START}event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"input_tokens\":null,\
         \"cache_creation_input_tokens\":null,\"cache_read_input_tokens\":null,\"output_tokens\":300}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    );
    let info = Http11Parser
        .parse_response(&http11_sse(&body))
        .expect("usage");
    assert_eq!((info.prompt_tokens, info.completion_tokens), (102_050, 300));
    assert_eq!(cache(&info), (Some(100_000), Some(2000)));
}

#[test]
fn anthropic_sse_keeps_message_start_speed_geo_and_1h_writes() {
    // message_delta usage has no speed, inference_geo or cache_creation; null must not erase them.
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":50,",
        "\"cache_creation_input_tokens\":2000,\"cache_read_input_tokens\":0,\"cache_creation\":{\"ephemeral_5m_input_tokens\":500,",
        "\"ephemeral_1h_input_tokens\":1500},\"output_tokens\":1,\"speed\":\"fast\",\"inference_geo\":\"us\"}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":50,",
        "\"cache_creation_input_tokens\":2000,\"cache_read_input_tokens\":0,\"output_tokens\":300,\"speed\":null}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let info = Http11Parser
        .parse_response(&http11_sse(body))
        .expect("usage");
    assert_eq!((info.prompt_tokens, info.completion_tokens), (2050, 300));
    assert_eq!(info.cache_write_long_ttl_tokens, Some(1500));
    assert_eq!(
        (info.speed.as_deref(), info.inference_geo.as_deref()),
        (Some("fast"), Some("us"))
    );
}

#[test]
fn responses_sse_reads_served_tier_and_cache_writes_from_response_completed() {
    let body = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-6.1-sol\",\"service_tier\":\"auto\",",
        "\"status\":\"in_progress\",\"error\":null,\"usage\":null}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-6.1-sol\",\"service_tier\":\"default\",",
        "\"status\":\"completed\",\"error\":null,\"usage\":{\"input_tokens\":12000,\"output_tokens\":500,",
        "\"input_tokens_details\":{\"cached_tokens\":4000,\"cache_write_tokens\":7000,",
        "\"cache_write_tokens_details\":{\"ttl_30m_tokens\":3000,\"ttl_12h_tokens\":4000}}}}}\n\n",
    );
    let info = Http11Parser
        .parse_response(&http11_sse(body))
        .expect("usage");
    assert_eq!(cache(&info), (Some(4000), Some(7000)));
    assert_eq!(info.cache_write_long_ttl_tokens, Some(4000));
    assert_eq!(info.service_tier.as_deref(), Some("default"));
}

#[test]
fn openai_chat_sse_reads_service_tier_from_the_usage_chunk() {
    let body = concat!(
        "data: {\"model\":\"gpt-6.1-sol\",\"service_tier\":\"flex\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
        "data: {\"model\":\"gpt-6.1-sol\",\"service_tier\":\"flex\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );
    let info = Http11Parser
        .parse_response(&http11_sse(body))
        .expect("usage");
    assert_eq!(info.service_tier.as_deref(), Some("flex"));
}

#[test]
fn openai_chat_sse_cached_tokens_stay_inside_prompt() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":2000,\"completion_tokens\":40,",
        "\"prompt_tokens_details\":{\"cached_tokens\":1536}}}\n\n",
        "data: [DONE]\n\n",
    );
    let info = Http11Parser
        .parse_response(&http11_sse(body))
        .expect("usage");
    assert_eq!((info.prompt_tokens, info.completion_tokens), (2000, 40));
    assert_eq!(cache(&info), (Some(1536), None));
}

#[test]
fn gemini_sse_reads_cached_content_tokens() {
    let body = concat!(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"i\"}]},\"finishReason\":\"STOP\"}],",
        "\"usageMetadata\":{\"promptTokenCount\":5000,\"candidatesTokenCount\":20,\"cachedContentTokenCount\":4096},",
        "\"modelVersion\":\"gemini-test\"}\r\n\r\n",
    );
    let info = Http11Parser
        .parse_response(&http11_sse(body))
        .expect("usage");
    assert_eq!((info.prompt_tokens, info.completion_tokens), (5000, 20));
    assert_eq!(cache(&info), (Some(4096), None));
}

#[test]
fn gemini_sse_reads_usage_metadata() {
    let body = concat!(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"h\"}]}}],\"usageMetadata\":{\"promptTokenCount\":7},\"modelVersion\":\"gemini-test\"}\r\n\r\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"i\"}]},\"finishReason\":\"STOP\"}],",
        "\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":3},\"modelVersion\":\"gemini-test\"}\r\n\r\n",
    );
    let info = Http11Parser
        .parse_response(&http11_sse(body))
        .expect("usage");
    assert_eq!((info.prompt_tokens, info.completion_tokens), (7, 3));
    assert_eq!(info.model.as_deref(), Some("gemini-test"));
}

#[test]
fn gemini_sse_zero_output_is_recorded() {
    for finish in ["MAX_TOKENS", "STOP"] {
        let body = format!(
            "data: {{\"candidates\":[{{\"content\":{{\"role\":\"model\"}},\"finishReason\":\"{finish}\",\"index\":0}}],\
             {GEMINI_ZERO_USAGE},\"modelVersion\":\"gemini-test\"}}\r\n\r\n"
        );
        let info = Http11Parser.parse_response(&http11_sse(&body));
        assert_eq!(tokens(info), Some((1, 0)), "{finish}");
    }
}

#[test]
fn gemini_prompt_block_is_recorded_on_every_path() {
    let blocked = "{\"promptFeedback\":{\"blockReason\":\"SAFETY\"},\
        \"usageMetadata\":{\"promptTokenCount\":8,\"totalTokenCount\":8},\"modelVersion\":\"gemini-test\"}";
    let json = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{blocked}");
    let sse = http11_sse(&format!("data: {blocked}\r\n\r\n"));
    let h2 = format!("data: {blocked}\r\n\r\n");

    assert_eq!(
        tokens(Http11Parser.parse_response(json.as_bytes())),
        Some((8, 0))
    );
    assert_eq!(tokens(Http11Parser.parse_response(&sse)), Some((8, 0)));
    assert_eq!(
        tokens(Http2Parser.parse_response(h2.as_bytes())),
        Some((8, 0))
    );
}

#[test]
fn gemini_tool_use_prompt_tokens_are_recorded_on_every_path() {
    let first = "{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"\\\"toolUsePromptTokenCount\\\":1\"}]}}],\
        \"usageMetadata\":{\"promptTokenCount\":27},\"modelVersion\":\"gemini-test\"}";
    let last = "{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"i\"}]},\"finishReason\":\"STOP\"}],\
        \"usageMetadata\":{\"promptTokenCount\":27,\"candidatesTokenCount\":120,\
        \"toolUsePromptTokenCount\":10309,\"totalTokenCount\":10456},\"modelVersion\":\"gemini-test\"}";
    let json = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{last}");
    let sse = http11_sse(&format!("data: {first}\r\n\r\ndata: {last}\r\n\r\n"));
    let h2 = format!("data: {first}\r\n\r\ndata: {last}\r\n\r\n");

    let want = Some((10_336, 120));
    assert_eq!(tokens(Http11Parser.parse_response(json.as_bytes())), want);
    assert_eq!(tokens(Http11Parser.parse_response(&sse)), want);
    assert_eq!(tokens(Http2Parser.parse_response(h2.as_bytes())), want);
}

#[test]
fn gemini_sse_partial_last_event_is_incomplete() {
    let body = concat!(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"h\"}]}}],\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":2}}\r\n\r\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"i\"}]},\"finishReason\":\"STOP\"}],\"usageMet",
    );
    let info = Http11Parser.parse_response(&http11_sse(body));
    assert_eq!(tokens(info), None);
}

#[test]
fn responses_sse_reads_usage_from_response_completed() {
    let body = format!(
        "{RESPONSES_CREATED}event: response.completed\n\
         data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"r1\",\"model\":\"gpt-6\",\"status\":\"completed\",\"error\":null,\
         \"usage\":{{\"input_tokens\":30,\"input_tokens_details\":{{\"cached_tokens\":5}},\"output_tokens\":9,\
         \"output_tokens_details\":{{\"reasoning_tokens\":2}},\"total_tokens\":39}}}}}}\n\n"
    );
    let info = Http11Parser
        .parse_response(&http11_sse(&body))
        .expect("usage");
    assert_eq!((info.prompt_tokens, info.completion_tokens), (30, 9));
    assert_eq!(
        (info.thoughts_tokens, info.cache_read_tokens),
        (Some(2), Some(5))
    );
    assert_eq!(info.model.as_deref(), Some("gpt-6"));
}

#[test]
fn responses_sse_incomplete_keeps_usage() {
    let body = format!(
        "{RESPONSES_CREATED}event: response.incomplete\n\
         data: {{\"type\":\"response.incomplete\",\"response\":{{\"model\":\"gpt-6\",\"status\":\"incomplete\",\"error\":null,\
         \"incomplete_details\":{{\"reason\":\"max_output_tokens\"}},\"usage\":{{\"input_tokens\":30,\"output_tokens\":16}}}}}}\n\n"
    );
    let info = Http11Parser.parse_response(&http11_sse(&body));
    assert_eq!(tokens(info), Some((30, 16)));
}

#[test]
fn responses_sse_failed_without_usage_is_error() {
    let body = format!(
        "{RESPONSES_CREATED}event: response.failed\n\
         data: {{\"type\":\"response.failed\",\"response\":{{\"model\":\"gpt-6\",\"status\":\"failed\",\
         \"error\":{{\"code\":\"server_error\",\"message\":\"boom\"}},\"usage\":null}}}}\n\n"
    );
    let info = Http11Parser.parse_response(&http11_sse(&body));
    assert_eq!(tokens(info), Some((0, 0)));
}

#[test]
fn responses_sse_without_terminal_event_is_incomplete() {
    let info = Http11Parser.parse_response(&http11_sse(RESPONSES_CREATED));
    assert_eq!(tokens(info), None);
}

#[test]
fn sse_string_response_field_keeps_top_level_usage() {
    let body = concat!(
        "data: {\"response\":\"\",\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n",
        "data: [DONE]\n\n",
    );
    let info = Http11Parser.parse_response(&http11_sse(body));
    assert_eq!(tokens(info), Some((5, 3)));
}
