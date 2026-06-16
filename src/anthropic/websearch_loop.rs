//! web_search local agentic loop
//!
//! Handles the case "after mixed tools (web_search + exec...) fall onto the normal chat path, the upstream returns a tool_use with name=web_search":
//! kiro-rs internally calls /mcp to search -> feeds the results back as a tool_result -> reconverts and resends -> loops until the upstream stops asking to search;
//! tool_use calls other than web_search (exec, etc.) are returned to the client as usual: they do not enter the loop and are not swallowed.
//!
//! Reuses: converter::convert_request (feedback), provider.call_api_stream, EventStreamDecoder,
//! websearch::{create_mcp_request, call_mcp_api, parse_search_results, generate_search_summary}。

use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::provider::KiroProvider;
use crate::model::config::ToolCompatibilityMode;
use crate::token;

use super::cache_metering::CacheUsagePlan;
use super::converter::{ConversionError, get_context_window_size};
use super::handlers::{RequestTracer, convert_request_with_mode_blocking, last_attempt_outcome};
use super::handlers::{UsageRecordHook, map_provider_error};
use super::stream::{SseEvent, THINKING_SIGNATURE_PLACEHOLDER, ToolJsonAccumulator};
use super::types::{ErrorResponse, Message, MessagesRequest};
use super::websearch::{self, WebSearchResults};

/// Maximum number of search rounds, to prevent an infinite loop if the upstream keeps asking to search
const MAX_WEB_SEARCH_ROUNDS: usize = 5;

type SseBytes = Result<Bytes, Infallible>;

enum StreamStartup {
    Started,
    Failed(Response),
}

struct StreamFirstByteMarker {
    tx: mpsc::Sender<SseBytes>,
    startup_tx: Option<oneshot::Sender<StreamStartup>>,
    started: bool,
}

impl StreamFirstByteMarker {
    fn new(tx: mpsc::Sender<SseBytes>, startup_tx: oneshot::Sender<StreamStartup>) -> Self {
        Self {
            tx,
            startup_tx: Some(startup_tx),
            started: false,
        }
    }

    async fn mark_first_upstream_chunk(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        let _ = self.tx.send(Ok(create_ping_sse())).await;
        if let Some(tx) = self.startup_tx.take() {
            let _ = tx.send(StreamStartup::Started);
        }
    }

    fn mark_started_before_final_flush(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        if let Some(tx) = self.startup_tx.take() {
            let _ = tx.send(StreamStartup::Started);
        }
    }

    fn fail_before_start(&mut self, response: Response) -> bool {
        if self.started {
            return false;
        }
        self.started = true;
        if let Some(tx) = self.startup_tx.take() {
            let _ = tx.send(StreamStartup::Failed(response));
        }
        true
    }
}

fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

fn create_error_sse(error_type: &str, message: impl Into<String>) -> Bytes {
    Bytes::from(
        SseEvent::new(
            "error",
            json!({
                "type": "error",
                "error": {
                    "type": error_type,
                    "message": message.into(),
                }
            }),
        )
        .to_sse_string(),
    )
}

/// Result of buffer-decoding one round of the upstream response
struct RoundOutcome {
    /// Accumulated assistant text
    text: String,
    /// Accumulated model reasoning text from reasoningContentEvent
    reasoning: String,
    /// Accumulated encrypted/redacted reasoning payloads from reasoningContentEvent
    redacted_reasoning: String,
    /// The complete tool_use for this round (name already restored via tool_name_map)
    tool_uses: Vec<DecodedToolUse>,
    /// Actual input tokens computed from contextUsageEvent
    context_input_tokens: Option<i32>,
    /// Cumulative credits from meteringEvent
    credits: f64,
    /// stop_reason override (max_tokens / model_context_window_exceeded)
    stop_reason_override: Option<String>,
    /// True if the upstream stream ended due to a read error, so the decoded
    /// content for this round is partial and must not be treated as a success.
    stream_error: bool,
    /// Invalid upstream tool JSON error, if any.
    tool_json_error: Option<String>,
}

impl RoundOutcome {
    fn from_tool_accumulator_finish(
        mut tool_accumulator: ToolJsonAccumulator,
        tool_name_map: &std::collections::HashMap<String, String>,
        mut outcome: Self,
    ) -> Self {
        outcome.text = crate::kiro::model::events::strip_tool_use_xml_leaks(&outcome.text);
        if outcome.tool_json_error.is_none() {
            match tool_accumulator.finish(tool_name_map) {
                Ok(pending) => {
                    // 空参数工具（EnterPlanMode 等）补发为合法 tool_use，纳入本轮 tool_uses。
                    for completed in pending {
                        outcome.tool_uses.push(DecodedToolUse {
                            id: completed.id,
                            name: completed.name,
                            input: completed.input,
                        });
                    }
                }
                Err(e) => {
                    tracing::error!("{}", e);
                    outcome.tool_json_error = Some(e.message());
                }
            }
        }
        outcome
    }
}

/// A fully decoded tool_use
struct DecodedToolUse {
    id: String,
    name: String,
    input: Value,
}

impl DecodedToolUse {
    fn query(&self) -> String {
        self.input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }
}

/// Decides whether this round should keep searching (enter the next loop round)
///
/// Continue condition: every tool_use this round is web_search (at least one) and the round limit has not been reached.
/// As soon as a client tool such as exec is mixed in, there is no tool_use at all, or the limit is reached, it stops and flushes (exec is never swallowed).
fn should_search_round(round_idx: usize, tool_uses: &[DecodedToolUse]) -> bool {
    let only_web_search = !tool_uses.is_empty() && tool_uses.iter().all(|t| t.name == "web_search");
    only_web_search && round_idx < MAX_WEB_SEARCH_ROUNDS
}

/// Buffer-decode one round of the upstream streaming response
async fn decode_round(
    response: reqwest::Response,
    model: &str,
    tool_name_map: &std::collections::HashMap<String, String>,
    mut first_byte_marker: Option<&mut StreamFirstByteMarker>,
) -> RoundOutcome {
    let mut body_stream = response.bytes_stream();
    let mut decoder = EventStreamDecoder::new();

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut redacted_reasoning = String::new();
    let mut tool_accumulator = ToolJsonAccumulator::new();
    let mut tool_uses: Vec<DecodedToolUse> = Vec::new();
    let mut context_input_tokens: Option<i32> = None;
    let mut credits = 0.0;
    let mut stop_reason_override: Option<String> = None;
    let mut stream_error = false;
    let mut tool_json_error: Option<String> = None;

    while let Some(chunk) = body_stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("web_search loop failed to read the response stream: {}", e);
                stream_error = true;
                break;
            }
        };
        if let Some(marker) = first_byte_marker.as_deref_mut() {
            marker.mark_first_upstream_chunk().await;
        }
        if let Err(e) = decoder.feed(&chunk) {
            tracing::warn!("buffer overflow: {}", e);
        }
        for result in decoder.decode_iter() {
            let frame = match result {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("failed to decode event: {}", e);
                    continue;
                }
            };
            let event = match Event::from_frame(frame) {
                Ok(ev) => ev,
                Err(_) => continue,
            };
            match event {
                Event::AssistantResponse(resp) => text.push_str(&resp.content),
                Event::Code(resp) => text.push_str(&resp.content),
                Event::ReasoningContent(rc) => {
                    if !rc.text.is_empty() {
                        reasoning.push_str(&rc.text);
                    } else if let Some(redacted) = rc.redacted_content.as_ref() {
                        redacted_reasoning.push_str(redacted);
                    }
                }
                Event::ToolUse(tu) => match tool_accumulator.push(&tu, tool_name_map) {
                    Ok(Some(completed)) => {
                        tool_uses.push(DecodedToolUse {
                            id: completed.id,
                            name: completed.name,
                            input: completed.input,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!("{}", e);
                        tool_json_error = Some(e.message());
                    }
                },
                Event::ContextUsage(cu) => {
                    let window = get_context_window_size(model);
                    let actual = (cu.context_usage_percentage * (window as f64) / 100.0) as i32;
                    context_input_tokens = Some(actual);
                    if cu.context_usage_percentage >= 100.0 {
                        stop_reason_override = Some("model_context_window_exceeded".to_string());
                    }
                }
                Event::Metering(m) => credits += m.usage,
                Event::Exception { exception_type, .. } => {
                    if exception_type == "ContentLengthExceededException" {
                        stop_reason_override = Some("max_tokens".to_string());
                    }
                }
                _ => {}
            }
        }
    }

    RoundOutcome::from_tool_accumulator_finish(
        tool_accumulator,
        tool_name_map,
        RoundOutcome {
            text,
            reasoning,
            redacted_reasoning,
            tool_uses,
            context_input_tokens,
            credits,
            stop_reason_override,
            stream_error,
            tool_json_error,
        },
    )
}

/// Run one upstream round (convert + streaming request + buffer decode)
///
/// On upstream/conversion failure, returns Err(an already-constructed pass-through error Response)
async fn run_round(
    provider: &Arc<KiroProvider>,
    payload: &MessagesRequest,
    hook: &UsageRecordHook,
    fallback_input_tokens: i32,
    tool_compatibility_mode: ToolCompatibilityMode,
    first_byte_marker: Option<&mut StreamFirstByteMarker>,
    tracer: &RequestTracer,
) -> Result<(RoundOutcome, u64), Response> {
    let conversion =
        match convert_request_with_mode_blocking(payload.clone(), tool_compatibility_mode).await {
            Ok(c) => c,
            Err(e) => {
                let (et, msg) = match &e {
                    ConversionError::UnsupportedModel(m) => {
                        ("invalid_request_error", format!("unsupported model: {}", m))
                    }
                    ConversionError::EmptyMessages => {
                        ("invalid_request_error", "message list is empty".to_string())
                    }
                    ConversionError::UnsupportedToolMapping(message) => {
                        ("unsupported_tool_mapping", message.clone())
                    }
                    ConversionError::InvalidImage(message) => {
                        ("invalid_request_error", message.clone())
                    }
                };
                hook.record(0, 0, 0, 0, 0, 0.0, "error");
                tracer.finalize(
                    "error",
                    Some(crate::admin::trace_db::outcome::BAD_REQUEST),
                    Some(&msg),
                    None,
                );
                return Err(
                    (StatusCode::BAD_REQUEST, Json(ErrorResponse::new(et, msg))).into_response()
                );
            }
        };

    let kiro_request = KiroRequest {
        conversation_state: conversion.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion.additional_model_request_fields,
    };
    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(b) => b,
        Err(e) => {
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                Some(crate::admin::trace_db::outcome::UNKNOWN),
                Some(&format!("failed to serialize request: {}", e)),
                None,
            );
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("failed to serialize request: {}", e),
                )),
            )
                .into_response());
        }
    };

    let call_result = match provider.call_api_stream(&request_body, Some(tracer)).await {
        Ok(r) => r,
        Err(e) => {
            hook.record(0, fallback_input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                last_attempt_outcome(tracer),
                Some(&e.to_string()),
                None,
            );
            return Err(map_provider_error(e));
        }
    };
    let credential_id = call_result.credential_id;
    let outcome = decode_round(
        call_result.response,
        &payload.model,
        &conversion.tool_name_map,
        first_byte_marker,
    )
    .await;
    if let Some(message) = &outcome.tool_json_error {
        hook.record(0, fallback_input_tokens, 0, 0, 0, 0.0, "error");
        tracer.finalize("error", last_attempt_outcome(tracer), Some(message), None);
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(
                "upstream_tool_json_error",
                message.clone(),
            )),
        )
            .into_response());
    }
    if outcome.stream_error {
        // The upstream stream was cut off mid-round; the decoded content is partial,
        // so fail the round instead of feeding truncated text/tool_use back into the loop.
        hook.record(0, fallback_input_tokens, 0, 0, 0, 0.0, "error");
        tracer.finalize(
            "interrupted",
            Some(crate::admin::trace_db::outcome::STREAM_INTERRUPTED),
            Some("Upstream response stream ended unexpectedly during the web_search loop."),
            None,
        );
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(
                "upstream_error",
                "Upstream response stream ended unexpectedly during the web_search loop."
                    .to_string(),
            )),
        )
            .into_response());
    }
    Ok((outcome, credential_id))
}

/// Feeds one round of assistant(text + web_search tool_use) + user(tool_result) back into payload.messages,
/// and appends server_tool_use + web_search_tool_result blocks (Contract A fields) to the presentation.
///
/// `searched` corresponds one-to-one (same order) to `round.tool_uses`; the search has already been completed.
fn append_search_round(
    payload: &mut MessagesRequest,
    round: &RoundOutcome,
    searched: &[Option<WebSearchResults>],
    presentation: &mut Vec<Value>,
    thinking_enabled: bool,
) {
    // assistant: text + this round's web_search tool_use (Kiro history requires tool_use<->tool_result pairing)
    let mut assistant_content: Vec<Value> = Vec::new();
    if thinking_enabled && !round.reasoning.is_empty() {
        assistant_content.push(json!({
            "type": "thinking",
            "thinking": round.reasoning,
            "signature": THINKING_SIGNATURE_PLACEHOLDER
        }));
    }
    if thinking_enabled && !round.redacted_reasoning.is_empty() {
        assistant_content.push(json!({
            "type": "redacted_thinking",
            "data": round.redacted_reasoning
        }));
    }
    if !round.text.is_empty() {
        assistant_content.push(json!({"type": "text", "text": round.text}));
    }
    for tu in &round.tool_uses {
        assistant_content.push(json!({
            "type": "tool_use", "id": tu.id, "name": tu.name, "input": tu.input
        }));
    }
    payload.messages.push(Message {
        role: "assistant".to_string(),
        content: Value::Array(assistant_content),
    });

    // user: each web_search tool_use is paired with a tool_result (content = search summary, shown to the upstream)
    let mut user_content: Vec<Value> = Vec::new();
    for (tu, results) in round.tool_uses.iter().zip(searched.iter()) {
        let query = tu.query();
        let summary = websearch::generate_search_summary(&query, results);
        user_content.push(json!({
            "type": "tool_result", "tool_use_id": tu.id, "content": summary
        }));

        // Client presentation: server_tool_use + web_search_tool_result (Contract A)
        let (srv_id, _mcp) = websearch::create_mcp_request(&query);
        presentation.push(json!({
            "type": "server_tool_use", "id": srv_id, "name": "web_search",
            "input": {"query": query}
        }));
        // Contract A: web_search_tool_result has only type + content (no tool_use_id), consistent with generate_websearch_events
        presentation.push(json!({
            "type": "web_search_tool_result",
            "content": build_result_block(results)
        }));
    }
    payload.messages.push(Message {
        role: "user".to_string(),
        content: Value::Array(user_content),
    });
}

/// Converts search results into an array of web_search_result blocks (Contract A fields)
fn build_result_block(results: &Option<WebSearchResults>) -> Vec<Value> {
    match results {
        Some(r) => r
            .results
            .iter()
            .map(|item| {
                let page_age = item.published_date.and_then(|ms| {
                    chrono::DateTime::from_timestamp_millis(ms)
                        .map(|dt| dt.format("%B %-d, %Y").to_string())
                });
                json!({
                    "type": "web_search_result",
                    "title": item.title,
                    "url": item.url,
                    "encrypted_content": item.snippet.clone().unwrap_or_default(),
                    "page_age": page_age
                })
            })
            .collect(),
        None => vec![],
    }
}

struct WebSearchLoopSuccess {
    model: String,
    content: Vec<Value>,
    stop_reason: String,
    input_tokens: i32,
    output_tokens: i32,
    cache_creation_tokens: i32,
    cache_read_tokens: i32,
}

async fn run_web_search_loop_inner(
    provider: Arc<KiroProvider>,
    mut payload: MessagesRequest,
    hook: UsageRecordHook,
    tool_compatibility_mode: ToolCompatibilityMode,
    cache_plan: CacheUsagePlan,
    mut first_byte_marker: Option<&mut StreamFirstByteMarker>,
    tracer: std::sync::Arc<RequestTracer>,
) -> Result<WebSearchLoopSuccess, Response> {
    let fallback_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    let mut presentation: Vec<Value> = Vec::new();
    let mut last_credential_id: u64 = 0;
    let mut last_context_input: Option<i32> = None;
    let mut total_credits = 0.0;
    let thinking_enabled = payload.thinking.as_ref().is_some_and(|t| t.is_enabled());

    for round_idx in 0..=MAX_WEB_SEARCH_ROUNDS {
        let (round, credential_id) = match run_round(
            &provider,
            &payload,
            &hook,
            fallback_input_tokens,
            tool_compatibility_mode,
            first_byte_marker.as_deref_mut(),
            tracer.as_ref(),
        )
        .await
        {
            Ok(v) => v,
            Err(resp) => return Err(resp),
        };
        last_credential_id = credential_id;
        last_context_input = round.context_input_tokens.or(last_context_input);
        total_credits += round.credits;

        if should_search_round(round_idx, &round.tool_uses) {
            // Real search: if any one fails -> propagate the error, never silently turn it into "No results found"
            let mut searched: Vec<Option<WebSearchResults>> =
                Vec::with_capacity(round.tool_uses.len());
            for tu in &round.tool_uses {
                let (_id, mcp_request) = websearch::create_mcp_request(&tu.query());
                match websearch::call_mcp_api(&provider, &mcp_request).await {
                    Ok(resp) => searched.push(websearch::parse_search_results(&resp)),
                    Err(e) => {
                        tracing::warn!("web_search MCP call failed: {}", e);
                        hook.record(
                            last_credential_id,
                            fallback_input_tokens,
                            0,
                            0,
                            0,
                            total_credits,
                            "error",
                        );
                        tracer.finalize(
                            "error",
                            last_attempt_outcome(&tracer),
                            Some(&e.to_string()),
                            None,
                        );
                        return Err(map_provider_error(e));
                    }
                }
            }
            append_search_round(
                &mut payload,
                &round,
                &searched,
                &mut presentation,
                thinking_enabled,
            );
            continue;
        }

        // Terminate: this round is not "pure web_search", or the limit has been reached -> flush to the client
        let stop_reason = round.stop_reason_override.clone().unwrap_or_else(|| {
            if round.tool_uses.is_empty() {
                "end_turn".to_string()
            } else {
                "tool_use".to_string()
            }
        });
        let total_input = last_context_input.unwrap_or(fallback_input_tokens);
        let (final_input, cache_creation_tokens, cache_read_tokens) =
            cache_plan.split_against_total(total_input);

        // Final content: presentation blocks (per-round search) + final-round text + final-round tool_use (exec, etc., returned as-is)
        let mut content: Vec<Value> = presentation.clone();
        if thinking_enabled && !round.reasoning.is_empty() {
            content.push(json!({
                "type": "thinking",
                "thinking": round.reasoning,
                "signature": THINKING_SIGNATURE_PLACEHOLDER
            }));
        }
        if thinking_enabled && !round.redacted_reasoning.is_empty() {
            content.push(json!({
                "type": "redacted_thinking",
                "data": round.redacted_reasoning
            }));
        }
        if !round.text.is_empty() {
            content.push(json!({"type": "text", "text": round.text}));
        }
        for tu in &round.tool_uses {
            content.push(json!({
                "type": "tool_use", "id": tu.id, "name": tu.name, "input": tu.input
            }));
        }

        let output_tokens = token::estimate_output_tokens(&content);
        hook.record(
            last_credential_id,
            final_input,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
            total_credits,
            "success",
        );
        cache_plan.commit_success();
        tracer.set_usage(
            final_input,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
        );
        tracer.finalize("success", None, None, None);

        return Ok(WebSearchLoopSuccess {
            model: payload.model,
            content,
            stop_reason,
            input_tokens: final_input,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
        });
    }

    // Theoretically unreachable (the loop always returns)
    hook.record(
        last_credential_id,
        fallback_input_tokens,
        0,
        0,
        0,
        total_credits,
        "error",
    );
    tracer.finalize(
        "error",
        Some(crate::admin::trace_db::outcome::UNKNOWN),
        Some("web_search loop exited unexpectedly"),
        None,
    );
    Err((
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new(
            "internal_error",
            "web_search loop exited unexpectedly",
        )),
    )
        .into_response())
}

/// web_search loop entry point
///
/// `stream_client`: whether the client wants SSE (true) or a single JSON response (false).
pub(super) async fn run_web_search_loop(
    provider: Arc<KiroProvider>,
    payload: MessagesRequest,
    hook: UsageRecordHook,
    stream_client: bool,
    tool_compatibility_mode: ToolCompatibilityMode,
    cache_plan: CacheUsagePlan,
    tracer: std::sync::Arc<RequestTracer>,
) -> Response {
    if stream_client {
        return render_deferred_sse(
            provider,
            payload,
            hook,
            tool_compatibility_mode,
            cache_plan,
            tracer,
        )
        .await;
    }

    match run_web_search_loop_inner(
        provider,
        payload,
        hook,
        tool_compatibility_mode,
        cache_plan,
        None,
        tracer,
    )
    .await
    {
        Ok(success) => render_json(
            &success.model,
            success.content,
            &success.stop_reason,
            success.input_tokens,
            success.output_tokens,
            success.cache_creation_tokens,
            success.cache_read_tokens,
        ),
        Err(resp) => resp,
    }
}

async fn render_deferred_sse(
    provider: Arc<KiroProvider>,
    payload: MessagesRequest,
    hook: UsageRecordHook,
    tool_compatibility_mode: ToolCompatibilityMode,
    cache_plan: CacheUsagePlan,
    tracer: std::sync::Arc<RequestTracer>,
) -> Response {
    let (tx, rx) = mpsc::channel::<SseBytes>(32);
    let (startup_tx, startup_rx) = oneshot::channel::<StreamStartup>();

    tokio::spawn(async move {
        let mut marker = StreamFirstByteMarker::new(tx.clone(), startup_tx);
        let result = run_web_search_loop_inner(
            provider,
            payload,
            hook,
            tool_compatibility_mode,
            cache_plan,
            Some(&mut marker),
            tracer,
        )
        .await;

        match result {
            Ok(success) => {
                marker.mark_started_before_final_flush();
                for event in build_sse_events(
                    &success.model,
                    success.content,
                    &success.stop_reason,
                    success.input_tokens,
                    success.output_tokens,
                    success.cache_creation_tokens,
                    success.cache_read_tokens,
                ) {
                    if tx
                        .send(Ok(Bytes::from(event.to_sse_string())))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            Err(resp) => {
                if !marker.fail_before_start(resp) {
                    let _ = tx
                        .send(Ok(create_error_sse(
                            "api_error",
                            "web_search loop failed after the upstream stream had started",
                        )))
                        .await;
                }
            }
        }
    });

    match startup_rx.await {
        Ok(StreamStartup::Started) => {
            let stream = stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            });
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .header(header::CONNECTION, "keep-alive")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Ok(StreamStartup::Failed(resp)) => resp,
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(
                "upstream_error",
                "web_search loop ended before the response stream could start",
            )),
        )
            .into_response(),
    }
}

/// Single JSON response (non-streaming)
fn render_json(
    model: &str,
    content: Vec<Value>,
    stop_reason: &str,
    input_tokens: i32,
    output_tokens: i32,
    cache_creation_tokens: i32,
    cache_read_tokens: i32,
) -> Response {
    let body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "cache_creation_input_tokens": cache_creation_tokens,
            "cache_read_input_tokens": cache_read_tokens
        }
    });
    (StatusCode::OK, Json(body)).into_response()
}

/// Renders the final content array into a sequence of SSE events
fn build_sse_events(
    model: &str,
    content: Vec<Value>,
    stop_reason: &str,
    input_tokens: i32,
    output_tokens: i32,
    cache_creation_tokens: i32,
    cache_read_tokens: i32,
) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let message_id = format!("msg_{}", &Uuid::new_v4().to_string().replace('-', "")[..24]);

    events.push(SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": cache_creation_tokens,
                    "cache_read_input_tokens": cache_read_tokens
                }
            }
        }),
    ));

    for (index, block) in content.iter().enumerate() {
        let index = index as i32;
        let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match btype {
            "thinking" => {
                let thinking = block.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": {"type": "thinking", "thinking": ""}
                    }),
                ));
                if !thinking.is_empty() {
                    events.push(SseEvent::new(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta", "index": index,
                            "delta": {"type": "thinking_delta", "thinking": thinking}
                        }),
                    ));
                }
                events.push(SseEvent::new(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta", "index": index,
                        "delta": {"type": "signature_delta", "signature": THINKING_SIGNATURE_PLACEHOLDER}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop", "index": index
                    }),
                ));
            }
            "redacted_thinking" => {
                let data = block.get("data").and_then(|v| v.as_str()).unwrap_or("");
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": {"type": "redacted_thinking", "data": data}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop", "index": index
                    }),
                ));
            }
            "text" => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": {"type": "text", "text": ""}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta", "index": index,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop", "index": index
                    }),
                ));
            }
            "tool_use" => {
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let partial = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta", "index": index,
                        "delta": {"type": "input_json_delta", "partial_json": partial}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop", "index": index
                    }),
                ));
            }
            "server_tool_use" | "web_search_tool_result" => {
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": block
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop", "index": index
                    }),
                ));
            }
            _ => {}
        }
    }

    events.push(SseEvent::new(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason},
            "usage": {
                "output_tokens": output_tokens,
                "cache_creation_input_tokens": cache_creation_tokens,
                "cache_read_input_tokens": cache_read_tokens
            }
        }),
    ));
    events.push(SseEvent::new(
        "message_stop",
        json!({"type": "message_stop"}),
    ));

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::websearch::{WebSearchResult, WebSearchResults};

    fn tu(name: &str) -> DecodedToolUse {
        DecodedToolUse {
            id: format!("toolu_{}", name),
            name: name.to_string(),
            input: json!({"query": "rust 2026"}),
        }
    }

    // ---- should_search_round: hit / skip / limit reached ----

    #[test]
    fn round_with_only_web_search_continues() {
        // Hit: this round is all web_search and the limit is not reached -> keep searching
        let tools = vec![tu("web_search"), tu("web_search")];
        assert!(should_search_round(0, &tools));
        assert!(should_search_round(MAX_WEB_SEARCH_ROUNDS - 1, &tools));
    }

    #[test]
    fn round_with_exec_does_not_enter_loop() {
        // Skip: exec mixed in (not web_search) -> terminate, exec returned to the client as-is
        let mixed = vec![tu("web_search"), tu("exec")];
        assert!(!should_search_round(0, &mixed));
        // Same for exec-only
        let exec_only = vec![tu("exec")];
        assert!(!should_search_round(0, &exec_only));
    }

    #[test]
    fn round_with_no_tool_use_does_not_enter_loop() {
        // Skip: no tool_use at all (plain-text answer) -> terminate
        let empty: Vec<DecodedToolUse> = vec![];
        assert!(!should_search_round(0, &empty));
    }

    #[test]
    fn round_at_limit_stops_even_if_web_search() {
        // Limit reached: even if this round is all web_search, hitting the limit must stop (prevents an infinite loop)
        let tools = vec![tu("web_search")];
        assert!(!should_search_round(MAX_WEB_SEARCH_ROUNDS, &tools));
        assert!(!should_search_round(MAX_WEB_SEARCH_ROUNDS + 1, &tools));
    }

    // ---- build_result_block: search results -> Contract A web_search_result fields ----

    #[test]
    fn result_block_maps_contract_a_fields() {
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Rust 1.99".to_string(),
                url: "https://example.com/rust".to_string(),
                snippet: Some("Rust 1.99 released".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("rust".to_string()),
            error: None,
        };
        let block = build_result_block(&Some(results));
        assert_eq!(block.len(), 1);
        assert_eq!(block[0]["type"], "web_search_result");
        assert_eq!(block[0]["title"], "Rust 1.99");
        assert_eq!(block[0]["url"], "https://example.com/rust");
        assert_eq!(block[0]["encrypted_content"], "Rust 1.99 released");
    }

    #[test]
    fn result_block_none_is_empty() {
        // No results -> empty block (does not fabricate content)
        assert!(build_result_block(&None).is_empty());
    }

    #[test]
    fn round_outcome_reports_pending_tool_json_on_finish() {
        let mut acc = ToolJsonAccumulator::new();
        acc.push(
            &crate::kiro::model::events::ToolUseEvent {
                name: "web_search".to_string(),
                tool_use_id: "tool_pending".to_string(),
                input: r#"{"query":"kiro"#.to_string(),
                stop: false,
            },
            &std::collections::HashMap::new(),
        )
        .unwrap();

        let outcome = RoundOutcome::from_tool_accumulator_finish(
            acc,
            &std::collections::HashMap::new(),
            RoundOutcome {
                text: String::new(),
                reasoning: String::new(),
                redacted_reasoning: String::new(),
                tool_uses: Vec::new(),
                context_input_tokens: None,
                credits: 0.0,
                stop_reason_override: None,
                stream_error: false,
                tool_json_error: None,
            },
        );

        assert!(
            outcome
                .tool_json_error
                .as_deref()
                .unwrap()
                .contains("tool_pending")
        );
    }

    // ---- search-failure pass-through: an Err from the MCP call must map to an error response, never silently become a 200 "No results found" ----

    #[test]
    fn mcp_failure_maps_to_error_response_not_silent_success() {
        // When the loop gets Err from call_mcp_api it directly `return map_provider_error(e)`,
        // before any generate_search_summary, so a search failure can never turn into a successful summary response.
        // This verifies that map_provider_error returns a non-2xx (BAD_GATEWAY) for a generic MCP error,
        // rather than 200, proving the pass-through path cannot produce a false green.
        let err = anyhow::anyhow!("MCP error: -1 - upstream unavailable");
        let resp = map_provider_error(err);
        assert!(
            !resp.status().is_success(),
            "a failed MCP search must return an error status and must not silently succeed"
        );
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    // ---- build_sse_events: present server_tool_use + result, and the exec tool_use is not swallowed ----

    #[test]
    fn sse_events_render_search_presentation_and_keep_exec() {
        let content = vec![
            json!({"type": "server_tool_use", "id": "srvtoolu_x", "name": "web_search", "input": {"query": "q"}}),
            json!({"type": "web_search_tool_result", "content": []}),
            json!({"type": "text", "text": "done"}),
            json!({"type": "tool_use", "id": "toolu_exec", "name": "exec", "input": {"cmd": "ls"}}),
        ];
        let events = build_sse_events("claude-opus-4-8", content, "tool_use", 10, 5, 3, 2);

        // Must contain message_start / message_delta(stop_reason) / message_stop
        assert_eq!(events.first().unwrap().event, "message_start");
        assert_eq!(events.last().unwrap().event, "message_stop");
        let delta = events.iter().find(|e| e.event == "message_delta").unwrap();
        assert_eq!(delta.data["delta"]["stop_reason"], "tool_use");
        assert_eq!(delta.data["usage"]["cache_creation_input_tokens"], json!(3));
        assert_eq!(delta.data["usage"]["cache_read_input_tokens"], json!(2));

        // the server_tool_use block is placed into content_block_start as-is
        let has_server_tool = events.iter().any(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "server_tool_use"
        });
        assert!(
            has_server_tool,
            "the server_tool_use block should be presented"
        );

        // the web_search_tool_result block is presented
        let has_result = events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "web_search_tool_result"
        });
        assert!(
            has_result,
            "the web_search_tool_result block should be presented"
        );

        // exec tool_use is not swallowed: name=exec appears in start
        let has_exec = events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "tool_use"
                && e.data["content_block"]["name"] == "exec"
        });
        assert!(
            has_exec,
            "the exec tool_use must be returned to the client as-is and not swallowed"
        );
    }

    #[test]
    fn sse_events_render_redacted_thinking_without_plaintext_delta() {
        let content = vec![json!({
            "type": "redacted_thinking",
            "data": "encrypted-thinking"
        })];
        let events = build_sse_events("claude-opus-4-8", content, "end_turn", 10, 5, 0, 0);

        let start = events
            .iter()
            .find(|e| {
                e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "redacted_thinking"
            })
            .expect("redacted thinking block should be rendered");
        assert_eq!(
            start.data["content_block"]["data"].as_str(),
            Some("encrypted-thinking")
        );
        assert!(
            events.iter().all(|e| {
                !(e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta")
            }),
            "redacted_thinking must not be serialized as plaintext thinking_delta"
        );
    }

    #[test]
    fn sse_events_include_cache_usage_in_start_and_delta() {
        let events = build_sse_events(
            "claude-opus-4-8",
            vec![json!({"type": "text", "text": "done"})],
            "end_turn",
            123,
            7,
            45,
            67,
        );

        let start = events
            .iter()
            .find(|e| e.event == "message_start")
            .expect("message_start should exist");
        assert_eq!(
            start.data["message"]["usage"]["cache_creation_input_tokens"],
            json!(45)
        );
        assert_eq!(
            start.data["message"]["usage"]["cache_read_input_tokens"],
            json!(67)
        );

        let delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("message_delta should exist");
        assert_eq!(
            delta.data["usage"]["cache_creation_input_tokens"],
            json!(45)
        );
        assert_eq!(delta.data["usage"]["cache_read_input_tokens"], json!(67));
    }
}
