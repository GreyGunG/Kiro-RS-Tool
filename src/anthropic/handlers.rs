//! Anthropic API Handler 函数

use std::convert::Infallible;
use std::pin::Pin;
use std::time::Instant;

use crate::admin::client_keys::SharedClientKeyManager;
use crate::admin::trace_db::{SharedTraceStore, TraceAttempt, TraceRecord, TraceSink, outcome};
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder, UsageRecord};
use crate::image_resize::RequestImageLimits;
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;
use anyhow::Error;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::{Extension, State},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use chrono::Utc;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use super::cache_metering::CacheUsagePlan;
use super::converter::{ConversionError, ConversionResult, convert_request_with_mode};
use super::middleware::{AppState, KeyContext};
use super::stream::{BufferedStreamContext, SseEvent, StreamContext, ToolJsonAccumulator};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    OutputConfig, Thinking, normalize_thinking_effort,
};
use super::websearch;

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, Infallible>> + Send>>;

fn normalize_web_search_tool(payload: &mut MessagesRequest) {
    if let Some(tools) = payload.tools.as_mut() {
        for tool in tools {
            if tool.name == "WebSearch" {
                tool.name = "web_search".to_string();
                if tool.tool_type.is_none() {
                    tool.tool_type = Some("web_search_20250305".to_string());
                }
            }
        }
    }

    if let Some(choice) = payload.tool_choice.as_mut()
        && choice.get("name").and_then(|v| v.as_str()) == Some("WebSearch")
        && let Some(obj) = choice.as_object_mut()
    {
        obj.insert(
            "name".to_string(),
            serde_json::Value::String("web_search".to_string()),
        );
    }
}

/// 请求结束时记录用量的钩子
///
/// 在 handler 入口构造，调用 [`Self::record`] 时把当次请求的 input/output token、
/// 命中的上游凭据 ID、状态写入：
/// - `usage_log.YYYY-MM-DD.jsonl`（持久化历史）
/// - 内存聚合器（仪表盘趋势）
/// - 客户端 Key 计数（按 Key 累计）
#[derive(Clone)]
pub(crate) struct UsageRecordHook {
    pub recorder: Option<SharedRecorder>,
    pub aggregator: Option<SharedAggregator>,
    pub client_keys: Option<SharedClientKeyManager>,
    pub key_id: u64,
    pub model: String,
    pub started_at: Instant,
}

impl UsageRecordHook {
    pub fn from_state(state: &AppState, key_id: u64, model: String) -> Self {
        Self {
            recorder: state.usage_recorder.clone(),
            aggregator: state.usage_aggregator.clone(),
            client_keys: state.client_keys.clone(),
            key_id,
            model,
            started_at: Instant::now(),
        }
    }

    pub fn record(
        &self,
        credential_id: u64,
        input_tokens: i32,
        output_tokens: i32,
        cache_creation_tokens: i32,
        cache_read_tokens: i32,
        credits: f64,
        status: &str,
    ) {
        let rec = UsageRecord {
            ts: Utc::now().to_rfc3339(),
            key_id: self.key_id,
            credential_id,
            model: self.model.clone(),
            input_tokens: input_tokens.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            cache_creation_tokens: cache_creation_tokens.max(0) as u64,
            cache_read_tokens: cache_read_tokens.max(0) as u64,
            credits: if credits.is_finite() && credits > 0.0 {
                credits
            } else {
                0.0
            },
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            status: status.to_string(),
        };
        if let Some(r) = &self.recorder {
            r.record(&rec);
        }
        if let Some(a) = &self.aggregator {
            a.ingest(&rec);
        }
        if status == "success" && self.key_id != 0 {
            if let Some(m) = &self.client_keys {
                m.record_usage(
                    self.key_id,
                    rec.input_tokens,
                    rec.output_tokens,
                    rec.cache_creation_tokens,
                    rec.cache_read_tokens,
                    rec.credits,
                );
            }
        }
    }
}

/// 单次请求的链路追踪器
///
/// 在 handler 入口构造，作为 [`TraceSink`] 传入 provider；provider 在重试循环里
/// 每跳调用 [`on_attempt`](TraceSink::on_attempt) 累积一条 [`TraceAttempt`]。
/// 请求结束时调用 [`Self::finalize`] 组装 [`TraceRecord`] 并写入 SQLite。
///
/// `store` 为 None（未启用 Admin / trace）时所有方法都是空操作，零开销。
pub(crate) struct RequestTracer {
    store: Option<SharedTraceStore>,
    trace_id: String,
    ts: String,
    key_id: u64,
    model: String,
    is_stream: bool,
    started_at: Instant,
    attempts: parking_lot::Mutex<Vec<TraceAttempt>>,
    /// 最终 token 用量 (input, output, cache_creation, cache_read)，互斥口径。
    usage: parking_lot::Mutex<Option<(i32, i32, i32, i32)>>,
}

impl RequestTracer {
    pub fn new(state: &AppState, key_id: u64, model: String, is_stream: bool) -> Self {
        Self {
            store: state.trace_store.clone(),
            trace_id: Uuid::new_v4().to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id,
            model,
            is_stream,
            started_at: Instant::now(),
            attempts: parking_lot::Mutex::new(Vec::new()),
            usage: parking_lot::Mutex::new(None),
        }
    }

    /// 记录本次请求最终 token 用量。多次调用以最后一次为准。
    pub fn set_usage(&self, input: i32, output: i32, cache_creation: i32, cache_read: i32) {
        *self.usage.lock() = Some((input, output, cache_creation, cache_read));
    }

    /// 组装并落库一条完整链路。store 为 None 时不做任何事。
    pub fn finalize(
        &self,
        final_status: &str,
        error_type: Option<&str>,
        error_message: Option<&str>,
        interrupted_after_bytes: Option<u64>,
    ) {
        let Some(store) = &self.store else { return };
        let attempts = std::mem::take(&mut *self.attempts.lock());
        // 最终凭据：最后一跳的命中凭据（成功跳即命中凭据，失败跳即最后尝试的凭据）
        let final_credential_id = attempts.last().map(|a| a.credential_id).unwrap_or(0);
        let (input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens) =
            self.usage.lock().unwrap_or((0, 0, 0, 0));
        let rec = TraceRecord {
            trace_id: self.trace_id.clone(),
            ts: self.ts.clone(),
            key_id: self.key_id,
            model: self.model.clone(),
            is_stream: self.is_stream,
            final_status: final_status.to_string(),
            final_credential_id,
            error_type: error_type.map(|s| s.to_string()),
            error_message: error_message.map(crate::security::redact_text),
            total_attempts: attempts.len() as u32,
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            interrupted_after_bytes,
            input_tokens: input_tokens.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            cache_creation_tokens: cache_creation_tokens.max(0) as u64,
            cache_read_tokens: cache_read_tokens.max(0) as u64,
            attempts,
        };
        store.insert(&rec);
    }
}

impl TraceSink for RequestTracer {
    fn on_attempt(&self, attempt: TraceAttempt) {
        self.attempts.lock().push(attempt);
    }
}

/// 取追踪器里最后一跳的 outcome（用于把 provider 的失败分类提升到 record.error_type）。
/// 返回 'static str（outcome 常量），无 attempt 时返回 None。
pub(super) fn last_attempt_outcome(tracer: &RequestTracer) -> Option<&'static str> {
    let last = tracer.attempts.lock().last()?.outcome.clone();
    Some(match last.as_str() {
        outcome::QUOTA_EXHAUSTED => outcome::QUOTA_EXHAUSTED,
        outcome::ACCOUNT_THROTTLED => outcome::ACCOUNT_THROTTLED,
        outcome::AUTH_FAILED => outcome::AUTH_FAILED,
        outcome::TRANSIENT => outcome::TRANSIENT,
        outcome::NETWORK_ERROR => outcome::NETWORK_ERROR,
        outcome::BAD_REQUEST => outcome::BAD_REQUEST,
        _ => outcome::UNKNOWN,
    })
}

/// Image-budget warning threshold (in raw base64 chars, not decoded bytes).
/// Emits a warning when the total base64 char count of all image content in one request exceeds this threshold.
/// The threshold does not reject the request (the upstream makes the final call); it only gives operators more precise diagnostics.
const IMAGE_BUDGET_WARN_BYTES: usize = 800 * 1024;

/// Budget statistics for the image content in one inbound request.
#[derive(Debug, Clone, Copy)]
struct ImageBudget {
    count: usize,
    total_b64_bytes: usize,
    largest_b64_bytes: usize,
}

/// Counts the total number of images in the payload and their base64 byte size.
/// Looks only at inline base64 (image source.type == "base64"), skipping url-mode images (which do not
/// go directly into a Bedrock single message body). This is a lightweight O(N) scan that does not decode base64.
fn count_image_budget(payload: &super::types::MessagesRequest) -> ImageBudget {
    let mut count = 0usize;
    let mut total = 0usize;
    let mut largest = 0usize;
    for msg in &payload.messages {
        if let serde_json::Value::Array(arr) = &msg.content {
            count_images_in_blocks(arr, &mut count, &mut total, &mut largest);
        }
    }
    ImageBudget {
        count,
        total_b64_bytes: total,
        largest_b64_bytes: largest,
    }
}

fn count_images_in_blocks(
    blocks: &[serde_json::Value],
    count: &mut usize,
    total: &mut usize,
    largest: &mut usize,
) {
    for item in blocks {
        match item.get("type").and_then(|v| v.as_str()) {
            Some("image") => count_image_source(item.get("source"), count, total, largest),
            Some("tool_result") => {
                if let Some(serde_json::Value::Array(content)) = item.get("content") {
                    count_images_in_blocks(content, count, total, largest);
                }
            }
            _ => {}
        }
    }
}

fn count_image_source(
    source: Option<&serde_json::Value>,
    count: &mut usize,
    total: &mut usize,
    largest: &mut usize,
) {
    let Some(src) = source else {
        return;
    };
    if src.get("type").and_then(|v| v.as_str()) != Some("base64") {
        return;
    }
    let n = src
        .get("data")
        .and_then(|v| v.as_str())
        .map(|s| s.len())
        .unwrap_or(0);
    *count += 1;
    *total += n;
    *largest = (*largest).max(n);
}

fn validate_image_budget_or_response(
    budget: &ImageBudget,
    hook: &UsageRecordHook,
) -> Option<Response> {
    match RequestImageLimits::from_env().validate(budget.count, budget.total_b64_bytes) {
        Ok(()) => None,
        Err(e) => {
            tracing::warn!(error = %e, "incoming image payload rejected by safety limits");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            Some(
                (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new("invalid_request_error", e.to_string())),
                )
                    .into_response(),
            )
        }
    }
}

pub(super) async fn convert_request_with_mode_blocking(
    payload: MessagesRequest,
    tool_compatibility_mode: crate::model::config::ToolCompatibilityMode,
) -> Result<ConversionResult, ConversionError> {
    match tokio::task::spawn_blocking(move || {
        convert_request_with_mode(&payload, tool_compatibility_mode)
    })
    .await
    {
        Ok(result) => result,
        Err(e) => Err(ConversionError::InvalidImage(format!(
            "image/request conversion task failed: {}",
            e
        ))),
    }
}

fn conversion_error_response(e: &ConversionError) -> (&'static str, String) {
    match e {
        ConversionError::UnsupportedModel(model) => {
            ("invalid_request_error", format!("模型不支持: {}", model))
        }
        ConversionError::EmptyMessages => ("invalid_request_error", "消息列表为空".to_string()),
        ConversionError::UnsupportedToolMapping(message) => {
            ("unsupported_tool_mapping", message.clone())
        }
        ConversionError::InvalidImage(message) => ("invalid_request_error", message.clone()),
    }
}

fn provider_error_status_and_detail(err: &Error) -> (StatusCode, &'static str, String) {
    let err_str = err.to_string();

    // 上下文窗口满了（对话历史累积超出模型上下文窗口限制）
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Context window is full. Reduce conversation history, system prompt, or tools."
                .to_string(),
        );
    }

    // 单次输入太长（请求体本身超出上游限制）
    if err_str.contains("Input is too long") {
        tracing::warn!(error = %err, "上游拒绝请求：输入过长（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Input is too long. Reduce the size of your messages.".to_string(),
        );
    }

    // Bedrock client-side validation errors (tool_use <-> tool_result mismatch, invalid message sequence, etc.)
    // The root cause is the client's own messages array, not an upstream failure, so it must not map to 5xx
    // otherwise it triggers an upstream cooldown that amplifies one client error into a 30+ burst of 503s.
    // Detection is centralized in the endpoint layer (single source of truth for the markers); the provider
    // already bails out without retry on these, and this mapping is the client-facing safety net.
    if crate::kiro::endpoint::default_is_client_validation_error(&err_str) {
        tracing::warn!(
            error = %err,
            "client messages array violates the protocol (Bedrock validation; mapped to 400 to avoid a false cooldown)"
        );
        // Return a stable, client-facing message and avoid echoing the raw upstream
        // error string (which can carry request IDs or internal validation details).
        // The full error is already logged above for diagnostics.
        return (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Invalid message sequence: tool_use and tool_result blocks must be correctly paired and ordered.".to_string(),
        );
    }

    let trace_hint = uuid::Uuid::new_v4().to_string();
    tracing::error!(
        trace_id = %trace_hint,
        error = %crate::security::redact_text(&err_str),
        "Kiro API 调用失败"
    );
    (
        StatusCode::BAD_GATEWAY,
        "api_error",
        format!("Upstream request failed. trace_id={trace_hint}"),
    )
}

/// 将 KiroProvider 错误映射为 HTTP 响应
pub(super) fn map_provider_error(err: Error) -> Response {
    let (status, error_type, message) = provider_error_status_and_detail(&err);
    (status, Json(ErrorResponse::new(error_type, message))).into_response()
}

/// 计算 Anthropic usage 口径的 input_tokens
fn resolve_usage_input_tokens(
    fallback_total_input_tokens: i32,
    context_total_input_tokens: Option<i32>,
) -> i32 {
    context_total_input_tokens.unwrap_or(fallback_total_input_tokens)
}

fn available_models() -> Vec<Model> {
    vec![
        Model {
            id: "claude-opus-4-8".to_string(),
            object: "model".to_string(),
            created: 1779897600, // May 28, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.8".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-8-thinking".to_string(),
            object: "model".to_string(),
            created: 1779897600, // May 28, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.8 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-7".to_string(),
            object: "model".to_string(),
            created: 1776276000, // Apr 16, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-7-thinking".to_string(),
            object: "model".to_string(),
            created: 1776276000, // Apr 16, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101-thinking".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-5-20250929".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-5-20250929-thinking".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001-thinking".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
    ]
}

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models() -> impl IntoResponse {
    tracing::info!("Received GET /v1/models request");

    let models = available_models();

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    // Count the image budget on inbound to provide precise diagnostics for later context-window-full errors
    let img_stats = count_image_budget(&payload);
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        image_count = %img_stats.count,
        image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
        image_largest_b64_kb = %(img_stats.largest_b64_bytes / 1024),
        "Received POST /v1/messages request"
    );
    if img_stats.total_b64_bytes > IMAGE_BUDGET_WARN_BYTES {
        tracing::warn!(
            image_count = %img_stats.count,
            image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
            "incoming image payload is large; if upstream rejects with CONTENT_LENGTH_EXCEEDS_THRESHOLD, reduce image count or use lower-resolution screenshots"
        );
    }
    let hook = UsageRecordHook::from_state(&state, key_ctx.key_id, payload.model.clone());
    if let Some(resp) = validate_image_budget_or_response(&img_stats, &hook) {
        return resp;
    }
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);
    normalize_web_search_tool(&mut payload);

    // CacheMeter 必须在 WebSearch 分流前计算；Claude Code 常驻 WebSearch 工具时
    // 会进入 agentic loop，同样需要向客户端上报 cache usage。
    let total_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;
    let cache_plan = state
        .cache_meter
        .as_ref()
        .map(|cache| super::cache_metering::compute_cache_usage(cache, &payload, key_ctx.key_id))
        .unwrap_or_default();

    let payload_stream = payload.stream;

    // 检查是否为 direct WebSearch 请求。真实 Claude Code 会把动态 system-reminder
    // 放在首个 user text block，此时必须走 agentic loop，让 Kiro 模型自己产出 query。
    if websearch::has_web_search_tool(&payload) && websearch::is_direct_web_search_request(&payload)
    {
        tracing::info!("检测到 direct WebSearch 工具，路由到 MCP 快路径");
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            key_ctx.key_id,
            payload.model.clone(),
            payload_stream,
        ));

        let (input_tokens, cache_creation_tokens, cache_read_tokens) =
            cache_plan.split_against_total(total_input_tokens);

        let resp = websearch::handle_websearch_request(
            provider,
            &payload,
            input_tokens,
            cache_creation_tokens,
            cache_read_tokens,
        )
        .await;
        // WebSearch 路径走 MCP 端点，没有 credential_id 上下文，统一记 0
        let status = if resp.status().is_success() {
            "success"
        } else {
            "error"
        };
        if status == "success" {
            cache_plan.commit_success();
        }
        tracer.set_usage(input_tokens, 0, cache_creation_tokens, cache_read_tokens);
        tracer.finalize(status, None, None, None);
        hook.record(
            0,
            input_tokens,
            0,
            cache_creation_tokens,
            cache_read_tokens,
            0.0,
            status,
        );
        return resp;
    }

    // Mixed-tools (web_search + exec...) case: web_search coexists with other tools and falls onto the normal chat path,
    // where the upstream may return a tool_use with name=web_search. Take the internal agentic loop: search internally and feed the results back.
    if websearch::has_web_search_tool(&payload) || websearch::has_web_search_among_tools(&payload) {
        tracing::info!(
            "detected tools containing web_search, entering the web_search agentic loop"
        );
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            key_ctx.key_id,
            payload.model.clone(),
            payload_stream,
        ));
        return super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            payload_stream,
            state.tool_compatibility_mode,
            cache_plan,
            tracer,
        )
        .await;
    }

    // 转换请求
    let conversion_result =
        match convert_request_with_mode_blocking(payload.clone(), state.tool_compatibility_mode)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                let (error_type, message) = conversion_error_response(&e);
                tracing::warn!("请求转换失败: {}", e);
                hook.record(0, 0, 0, 0, 0, 0.0, "error");
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new(error_type, message)),
                )
                    .into_response();
            }
        };

    // Build the Kiro request. profile_arn is injected by the provider layer from the actual
    // credentials; additional_model_request_fields is already filtered by converter model support.
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion_result.additional_model_request_fields,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!(
        "Kiro request body: {}",
        crate::security::body_log_summary(&request_body)
    );

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    if payload.stream {
        // 流式响应
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            key_ctx.key_id,
            payload.model.clone(),
            true,
        ));
        handle_stream_request(
            provider,
            &request_body,
            &payload.model,
            total_input_tokens,
            thinking_enabled,
            tool_name_map,
            hook,
            cache_plan,
            tracer,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            key_ctx.key_id,
            payload.model.clone(),
            false,
        ));
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            total_input_tokens,
            extract_thinking,
            tool_name_map,
            hook,
            cache_plan,
            tracer,
        )
        .await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    hook: UsageRecordHook,
    cache_plan: CacheUsagePlan,
    tracer: std::sync::Arc<RequestTracer>,
) -> Response {
    let stream = create_deferred_sse_stream(
        provider,
        request_body.to_string(),
        model.to_string(),
        input_tokens,
        thinking_enabled,
        tool_name_map,
        hook,
        cache_plan,
        tracer,
    );

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

fn create_error_sse(error_type: &str, message: String) -> Bytes {
    Bytes::from(
        SseEvent::new(
            "error",
            json!({
                "type": "error",
                "error": {
                    "type": error_type,
                    "message": message,
                }
            }),
        )
        .to_sse_string(),
    )
}

#[allow(clippy::too_many_arguments)]
fn create_deferred_sse_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: String,
    model: String,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    hook: UsageRecordHook,
    cache_plan: CacheUsagePlan,
    tracer: std::sync::Arc<RequestTracer>,
) -> ByteStream {
    // CCH records TTFB when the first downstream byte arrives. Do not emit a
    // synthetic prelude here; the first downstream byte is delayed until the
    // upstream response body yields real bytes.
    let upstream = stream::once(async move {
        let call_result = match provider
            .call_api_stream(&request_body, Some(tracer.as_ref()))
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
                tracer.finalize(
                    "error",
                    last_attempt_outcome(&tracer),
                    Some(&e.to_string()),
                    None,
                );
                let (_, error_type, message) = provider_error_status_and_detail(&e);
                return Box::pin(stream::iter(vec![Ok(create_error_sse(
                    error_type, message,
                ))])) as ByteStream;
            }
        };

        let response = call_result.response;
        let credential_id = call_result.credential_id;

        let mut ctx =
            StreamContext::new_with_thinking(model, input_tokens, thinking_enabled, tool_name_map);
        ctx.cache_usage = cache_plan.usage();

        let initial_events = ctx.generate_initial_events();
        Box::pin(create_sse_stream(
            response,
            ctx,
            initial_events,
            hook,
            credential_id,
            tracer,
            cache_plan,
        )) as ByteStream
    })
    .flatten();

    Box::pin(upstream)
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
    cache_plan: CacheUsagePlan,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 初始事件不能在读到上游 body 前发送，否则 CCH 会把代理的占位字节当成
    // 模型 TTFB。这里将它们挂起，等首个真实 upstream chunk 到达后再一起发出。
    let body_stream = response.bytes_stream();

    let processing_stream = stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            hook,
            credential_id,
            tracer,
            0u64,
            cache_plan,
            Some(initial_events),
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, hook, credential_id, tracer, mut sent_bytes, cache_plan, mut pending_initial_events)| async move {
            if finished {
                return None;
            }

            // 使用 select! 同时等待数据和 ping 定时器
            tokio::select! {
                // 处理数据流
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            sent_bytes += chunk.len() as u64;
                            let mut events = pending_initial_events.take().unwrap_or_default();
                            if !events.is_empty() {
                                ping_interval.reset();
                            }

                            // 解码事件
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }

                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        if let Ok(event) = Event::from_frame(frame) {
                                            let sse_events = ctx.process_kiro_event(&event);
                                            events.extend(sse_events);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("解码事件失败: {}", e);
                                    }
                                }
                            }

                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, hook, credential_id, tracer, sent_bytes, cache_plan, pending_initial_events)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            // 发送最终事件并结束（记为 error）
                            let mut final_events = pending_initial_events.take().unwrap_or_default();
                            final_events.extend(ctx.generate_final_events());
                            record_stream_usage(&hook, &ctx, credential_id, "error", &tracer);
                            // 已开始返回内容后上游断流：标记为 interrupted，带已发送字节数
                            tracer.finalize(
                                "interrupted",
                                Some(outcome::STREAM_INTERRUPTED),
                                Some(&e.to_string()),
                                Some(sent_bytes),
                            );
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes, cache_plan, pending_initial_events)))
                        }
                        None => {
                            // 流结束，发送最终事件
                            let mut final_events = pending_initial_events.take().unwrap_or_default();
                            final_events.extend(ctx.generate_final_events());
                            let tool_json_error = ctx.tool_json_error_message();
                            if let Some(message) = tool_json_error.as_deref() {
                                record_stream_usage(&hook, &ctx, credential_id, "error", &tracer);
                                tracer.finalize("error", Some(outcome::BAD_REQUEST), Some(message), None);
                            } else {
                                record_stream_usage(&hook, &ctx, credential_id, "success", &tracer);
                                cache_plan.commit_success();
                                tracer.finalize("success", None, None, None);
                            }
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes, cache_plan, pending_initial_events)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick(), if pending_initial_events.is_none() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, hook, credential_id, tracer, sent_bytes, cache_plan, pending_initial_events)))
                }
            }
        },
    )
    .flatten();

    processing_stream
}

/// 从 StreamContext 提取最终用量，写入 hook，并同步给 tracer。
fn record_stream_usage(
    hook: &UsageRecordHook,
    ctx: &StreamContext,
    credential_id: u64,
    status: &str,
    tracer: &RequestTracer,
) {
    let (input, cache_creation, cache_read) = ctx.resolved_usage();
    hook.record(
        credential_id,
        input,
        ctx.output_tokens,
        cache_creation,
        cache_read,
        ctx.credits,
        status,
    );
    tracer.set_usage(input, ctx.output_tokens, cache_creation, cache_read);
}

use super::converter::get_context_window_size;

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    hook: UsageRecordHook,
    cache_plan: CacheUsagePlan,
    tracer: std::sync::Arc<RequestTracer>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let call_result = match provider.call_api(request_body, Some(tracer.as_ref())).await {
        Ok(resp) => resp,
        Err(e) => {
            hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&e.to_string()),
                None,
            );
            return map_provider_error(e);
        }
    };
    let response = call_result.response;
    let credential_id = call_result.credential_id;

    // 读取响应体
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("读取响应体失败: {}", e);
            hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "interrupted",
                Some(outcome::STREAM_INTERRUPTED),
                Some(&e.to_string()),
                None,
            );
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("读取响应失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut text_content = String::new();
    let mut reasoning_content = String::new();
    let mut redacted_reasoning_content = String::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;
    // meteringEvent 上报的 token 与缓存数据
    // 上游 metering 只给 credit；input 来自 contextUsage，output 来自估算。
    // 缓存 input/cache_* 的互斥分摊在拿到 total 真值后由 cache_usage 完成。
    let mut credits: f64 = 0.0;

    // 收集工具调用的增量 JSON，只有完整且可解析的 JSON 才返回给客户端
    let mut tool_accumulator = ToolJsonAccumulator::new();

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                if let Ok(event) = Event::from_frame(frame) {
                    match event {
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::Code(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::ReasoningContent(reasoning) => {
                            if !reasoning.text.is_empty() {
                                reasoning_content.push_str(&reasoning.text);
                            } else if let Some(redacted) = reasoning.redacted_content.as_ref() {
                                redacted_reasoning_content.push_str(redacted);
                            }
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;

                            match tool_accumulator.push(&tool_use, &tool_name_map) {
                                Ok(Some(completed)) => {
                                    tool_uses.push(json!({
                                        "type": "tool_use",
                                        "id": completed.id,
                                        "name": completed.name,
                                        "input": completed.input
                                    }));
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    tracing::error!("{}", e);
                                    let total_input = resolve_usage_input_tokens(
                                        input_tokens,
                                        context_input_tokens,
                                    );
                                    let (input, cache_creation, cache_read) =
                                        cache_plan.split_against_total(total_input);
                                    hook.record(
                                        credential_id,
                                        input,
                                        0,
                                        cache_creation,
                                        cache_read,
                                        credits,
                                        "error",
                                    );
                                    tracer.set_usage(input, 0, cache_creation, cache_read);
                                    tracer.finalize(
                                        "error",
                                        Some(outcome::BAD_REQUEST),
                                        Some(&e.message()),
                                        None,
                                    );
                                    return (
                                        StatusCode::BAD_GATEWAY,
                                        Json(ErrorResponse::new(e.error_type(), e.message())),
                                    )
                                        .into_response();
                                }
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            // 从上下文使用百分比计算实际的 input_tokens
                            let window_size = get_context_window_size(model);
                            let actual_input_tokens =
                                (context_usage.context_usage_percentage * (window_size as f64)
                                    / 100.0) as i32;
                            context_input_tokens = Some(actual_input_tokens);
                            // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                            if context_usage.context_usage_percentage >= 100.0 {
                                stop_reason = "model_context_window_exceeded".to_string();
                            }
                            tracing::debug!(
                                "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                                context_usage.context_usage_percentage,
                                actual_input_tokens
                            );
                        }
                        Event::Metering(metering) => {
                            // 上游只下发 credit；token / cache 字段不存在
                            credits += metering.usage;
                            tracing::debug!("metering credits +{:.6}", metering.usage);
                        }
                        Event::Exception { exception_type, .. } => {
                            if exception_type == "ContentLengthExceededException" {
                                stop_reason = "max_tokens".to_string();
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!("解码事件失败: {}", e);
            }
        }
    }

    match tool_accumulator.finish(&tool_name_map) {
        Ok(pending) => {
            // 空参数工具（EnterPlanMode 等）补发为合法 tool_use，与流式增量路径一致。
            for completed in pending {
                tool_uses.push(json!({
                    "type": "tool_use",
                    "id": completed.id,
                    "name": completed.name,
                    "input": completed.input
                }));
            }
        }
        Err(e) => {
            tracing::error!("{}", e);
            let total_input = resolve_usage_input_tokens(input_tokens, context_input_tokens);
            let (input, cache_creation, cache_read) = cache_plan.split_against_total(total_input);
            hook.record(
                credential_id,
                input,
                0,
                cache_creation,
                cache_read,
                credits,
                "error",
            );
            tracer.set_usage(input, 0, cache_creation, cache_read);
            tracer.finalize(
                "error",
                Some(outcome::BAD_REQUEST),
                Some(&e.message()),
                None,
            );
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(e.error_type(), e.message())),
            )
                .into_response();
        }
    }
    text_content = crate::kiro::model::events::strip_tool_use_xml_leaks(&text_content);

    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // 构建响应内容
    let mut content: Vec<serde_json::Value> = Vec::new();

    if thinking_enabled {
        let (thinking, remaining_text) = if !reasoning_content.is_empty() {
            (Some(reasoning_content), text_content)
        } else {
            // 从完整文本中提取 thinking 块
            let (thinking, remaining_text) =
                super::stream::extract_thinking_from_complete_text(&text_content);
            (thinking, remaining_text)
        };

        if let Some(thinking_text) = thinking {
            // signature 占位字符串：Kiro reasoning signature 不是 Anthropic
            // thinking signature，不能透传给下游。客户端只要求非空 signature
            // 以通过下一轮本地校验（"must be passed back"）。
            content.push(json!({
                "type": "thinking",
                "thinking": thinking_text,
                "signature": super::stream::THINKING_SIGNATURE_PLACEHOLDER,
            }));
        }

        if !redacted_reasoning_content.is_empty() {
            content.push(json!({
                "type": "redacted_thinking",
                "data": redacted_reasoning_content
            }));
        }

        if !remaining_text.is_empty() {
            content.push(json!({
                "type": "text",
                "text": remaining_text
            }));
        }
    } else {
        if !text_content.is_empty() {
            content.push(json!({
                "type": "text",
                "text": text_content
            }));
        }
    }

    content.extend(tool_uses);

    // 估算输出 tokens（上游不下发 token，全部走估算）
    let output_tokens = token::estimate_output_tokens(&content);

    // 全量 prompt token：contextUsage 真实值优先，否则用客户端估算。
    let total_input_tokens = resolve_usage_input_tokens(input_tokens, context_input_tokens);
    let (final_input_tokens, cache_creation_tokens, cache_read_tokens) =
        cache_plan.split_against_total(total_input_tokens);

    // 构建 Anthropic 响应
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": final_input_tokens,
            "output_tokens": output_tokens,
            "cache_creation_input_tokens": cache_creation_tokens,
            "cache_read_input_tokens": cache_read_tokens
        }
    });

    hook.record(
        credential_id,
        final_input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        credits,
        "success",
    );
    cache_plan.commit_success();
    tracer.set_usage(
        final_input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
    );
    tracer.finalize("success", None, None, None);
    (StatusCode::OK, Json(response_body)).into_response()
}

/// 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
///
/// - Kiro 原生 output_config 模型：覆写为 adaptive 类型
/// - 其他模型：覆写为 enabled 类型
/// - budget_tokens 固定为 20000
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    if !model_lower.contains("thinking") {
        if payload
            .thinking
            .as_ref()
            .is_some_and(|t| t.thinking_type == "adaptive")
            && payload.output_config.is_none()
        {
            payload.output_config = Some(OutputConfig {
                effort: normalize_thinking_effort("").to_string(),
            });
        }
        return;
    }

    let is_native_output_config_model = (model_lower.contains("opus")
        && (model_lower.contains("4-6")
            || model_lower.contains("4.6")
            || model_lower.contains("4-7")
            || model_lower.contains("4.7")
            || model_lower.contains("4-8")
            || model_lower.contains("4.8")))
        || (model_lower.contains("sonnet")
            && (model_lower.contains("4-6") || model_lower.contains("4.6")));

    let thinking_type = if is_native_output_config_model {
        "adaptive"
    } else {
        "enabled"
    };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });

    if is_native_output_config_model {
        let effort = parse_thinking_effort_from_model_name(&model_lower)
            .or_else(|| {
                payload
                    .output_config
                    .as_ref()
                    .map(|config| normalize_thinking_effort(&config.effort))
            })
            .unwrap_or_else(|| normalize_thinking_effort(""));
        payload.output_config = Some(OutputConfig {
            effort: effort.to_string(),
        });
    }
}

fn parse_thinking_effort_from_model_name(model_lower: &str) -> Option<&'static str> {
    for effort in ["max", "xhigh", "high", "medium", "low"] {
        let hyphen = format!("thinking-{effort}");
        let underscore = format!("thinking_{effort}");
        if model_lower.contains(&hyphen) || model_lower.contains(&underscore) {
            return Some(effort);
        }
    }
    None
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    Extension(_key_ctx): Extension<KeyContext>,
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
pub async fn post_messages_cc(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    let img_stats = count_image_budget(&payload);
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        image_count = %img_stats.count,
        image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
        image_largest_b64_kb = %(img_stats.largest_b64_bytes / 1024),
        "Received POST /cc/v1/messages request"
    );
    let hook = UsageRecordHook::from_state(&state, key_ctx.key_id, payload.model.clone());
    if let Some(resp) = validate_image_budget_or_response(&img_stats, &hook) {
        return resp;
    }

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);
    normalize_web_search_tool(&mut payload);

    // CacheMeter 必须在 WebSearch 分流前计算；/cc/v1/messages 常驻工具列表
    // 包含 WebSearch 时会进入 agentic loop，不能丢 cache usage。
    let total_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;
    let cache_plan = state
        .cache_meter
        .as_ref()
        .map(|cache| super::cache_metering::compute_cache_usage(cache, &payload, key_ctx.key_id))
        .unwrap_or_default();

    let payload_stream = payload.stream;

    // 检查是否为 direct WebSearch 请求。真实 Claude Code 会把动态 system-reminder
    // 放在首个 user text block，此时必须走 agentic loop，让 Kiro 模型自己产出 query。
    if websearch::has_web_search_tool(&payload) && websearch::is_direct_web_search_request(&payload)
    {
        tracing::info!("检测到 direct WebSearch 工具，路由到 MCP 快路径");
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            key_ctx.key_id,
            payload.model.clone(),
            payload_stream,
        ));

        let (input_tokens, cache_creation_tokens, cache_read_tokens) =
            cache_plan.split_against_total(total_input_tokens);

        let resp = websearch::handle_websearch_request(
            provider,
            &payload,
            input_tokens,
            cache_creation_tokens,
            cache_read_tokens,
        )
        .await;
        let status = if resp.status().is_success() {
            "success"
        } else {
            "error"
        };
        if status == "success" {
            cache_plan.commit_success();
        }
        tracer.set_usage(input_tokens, 0, cache_creation_tokens, cache_read_tokens);
        tracer.finalize(status, None, None, None);
        hook.record(
            0,
            input_tokens,
            0,
            cache_creation_tokens,
            cache_read_tokens,
            0.0,
            status,
        );
        return resp;
    }

    // Mixed-tools (web_search + exec...) case: web_search coexists with other tools and falls onto the normal chat path,
    // where the upstream may return a tool_use with name=web_search. Take the internal agentic loop: search internally and feed the results back.
    if websearch::has_web_search_tool(&payload) || websearch::has_web_search_among_tools(&payload) {
        tracing::info!(
            "detected tools containing web_search, entering the web_search agentic loop"
        );
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            key_ctx.key_id,
            payload.model.clone(),
            payload_stream,
        ));
        return super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            payload_stream,
            state.tool_compatibility_mode,
            cache_plan,
            tracer,
        )
        .await;
    }

    // 转换请求
    let conversion_result =
        match convert_request_with_mode_blocking(payload.clone(), state.tool_compatibility_mode)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                let (error_type, message) = conversion_error_response(&e);
                tracing::warn!("请求转换失败: {}", e);
                hook.record(0, 0, 0, 0, 0, 0.0, "error");
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new(error_type, message)),
                )
                    .into_response();
            }
        };

    // Build the Kiro request. profile_arn is injected by the provider layer from the actual
    // credentials; additional_model_request_fields is already filtered by converter model support.
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion_result.additional_model_request_fields,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!(
        "Kiro request body: {}",
        crate::security::body_log_summary(&request_body)
    );

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    if payload.stream {
        // 流式响应（缓冲模式）
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            key_ctx.key_id,
            payload.model.clone(),
            true,
        ));
        handle_stream_request_buffered(
            provider,
            &request_body,
            &payload.model,
            thinking_enabled,
            tool_name_map,
            hook,
            total_input_tokens,
            cache_plan,
            tracer,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            key_ctx.key_id,
            payload.model.clone(),
            false,
        ));
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            total_input_tokens,
            extract_thinking,
            tool_name_map,
            hook,
            cache_plan,
            tracer,
        )
        .await
    }
}

/// 处理流式请求（缓冲版本）
///
/// 与 `handle_stream_request` 不同，此函数会缓冲所有事件直到流结束，
/// 然后用从 contextUsageEvent 计算的正确 input_tokens 生成 message_start 事件。
async fn handle_stream_request_buffered(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    hook: UsageRecordHook,
    fallback_input_tokens: i32,
    cache_plan: CacheUsagePlan,
    tracer: std::sync::Arc<RequestTracer>,
) -> Response {
    let stream = create_deferred_buffered_sse_stream(
        provider,
        request_body.to_string(),
        model.to_string(),
        thinking_enabled,
        tool_name_map,
        hook,
        fallback_input_tokens,
        cache_plan,
        tracer,
    );

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn create_deferred_buffered_sse_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: String,
    model: String,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    hook: UsageRecordHook,
    fallback_input_tokens: i32,
    cache_plan: CacheUsagePlan,
    tracer: std::sync::Arc<RequestTracer>,
) -> ByteStream {
    // Buffered `/cc` streams also avoid pre-upstream placeholder bytes. A ping is
    // emitted only after the first real upstream body chunk has been read.
    let upstream = stream::once(async move {
        let call_result = match provider
            .call_api_stream(&request_body, Some(tracer.as_ref()))
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                hook.record(0, fallback_input_tokens, 0, 0, 0, 0.0, "error");
                tracer.finalize(
                    "error",
                    last_attempt_outcome(&tracer),
                    Some(&e.to_string()),
                    None,
                );
                let (_, error_type, message) = provider_error_status_and_detail(&e);
                return Box::pin(stream::iter(vec![Ok(create_error_sse(
                    error_type, message,
                ))])) as ByteStream;
            }
        };

        let response = call_result.response;
        let credential_id = call_result.credential_id;

        let mut ctx = BufferedStreamContext::new(
            model,
            fallback_input_tokens,
            thinking_enabled,
            tool_name_map,
        );
        ctx.set_cache_usage(cache_plan.usage());

        Box::pin(create_buffered_sse_stream(
            response,
            ctx,
            hook,
            credential_id,
            tracer,
            cache_plan,
        )) as ByteStream
    })
    .flatten();

    Box::pin(upstream)
}

/// 创建缓冲 SSE 事件流
///
/// 工作流程：
/// 1. 等待上游首个真实 body chunk 后发送 ping 标记首包，随后只发送 ping 保活信号
/// 2. 使用 StreamContext 的事件处理逻辑处理所有 Kiro 事件，结果缓存
/// 3. 流结束后，一次性发送剩余事件；工具 JSON 只在完整解析后发送
///
/// 注意：这里不能提前发送 `message_start`。`/cc/v1/messages` 的目的就是等待
/// `contextUsageEvent` 后修正首包 usage；首包一旦发出就无法再修改。
fn create_buffered_sse_stream(
    response: reqwest::Response,
    ctx: BufferedStreamContext,
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
    cache_plan: CacheUsagePlan,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            false,
            hook,
            credential_id,
            tracer,
            0u64,
            cache_plan,
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, mut first_upstream_chunk_seen, hook, credential_id, tracer, mut sent_bytes, cache_plan)| async move {
            if finished {
                return None;
            }

            loop {
                tokio::select! {
                    // 使用 biased 模式，优先检查 ping 定时器
                    // 避免在上游 chunk 密集时 ping 被"饿死"
                    biased;

                    // 优先检查 ping 保活（等待期间唯一发送的数据）
                    _ = ping_interval.tick(), if first_upstream_chunk_seen => {
                        tracing::trace!("发送 ping 保活事件（缓冲模式）");
                        let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, first_upstream_chunk_seen, hook, credential_id, tracer, sent_bytes, cache_plan)));
                    }

                    // 然后处理数据流
                    chunk_result = body_stream.next() => {
                        match chunk_result {
                            Some(Ok(chunk)) => {
                                sent_bytes += chunk.len() as u64;
                                // 解码事件
                                if let Err(e) = decoder.feed(&chunk) {
                                    tracing::warn!("缓冲区溢出: {}", e);
                                }

                                for result in decoder.decode_iter() {
                                    match result {
                                        Ok(frame) => {
                                            if let Ok(event) = Event::from_frame(frame) {
                                                // 缓冲事件（复用 StreamContext 的处理逻辑）
                                                ctx.process_and_buffer(&event);
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!("解码事件失败: {}", e);
                                        }
                                    }
                                }
                                if !first_upstream_chunk_seen {
                                    first_upstream_chunk_seen = true;
                                    ping_interval.reset();
                                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                                    return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, first_upstream_chunk_seen, hook, credential_id, tracer, sent_bytes, cache_plan)));
                                }
                                // 继续读取下一个 chunk，不发送任何数据
                            }
                            Some(Err(e)) => {
                                tracing::error!("读取响应流失败: {}", e);
                                // 发生错误，完成处理并返回所有事件
                                let all_events = ctx.finish_and_get_all_events();
                                let (i, o, cc, cr, credits) = ctx.final_usage();
                                hook.record(credential_id, i, o, cc, cr, credits, "error");
                                tracer.set_usage(i, o, cc, cr);
                                // 缓冲模式 chunk 读取失败：上游中途断流
                                tracer.finalize(
                                    "interrupted",
                                    Some(outcome::STREAM_INTERRUPTED),
                                    Some(&e.to_string()),
                                    Some(sent_bytes),
                                );
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, first_upstream_chunk_seen, hook, credential_id, tracer, sent_bytes, cache_plan)));
                            }
                            None => {
                                // 流结束，完成处理并返回所有事件（已更正 input_tokens）
                                let all_events = ctx.finish_and_get_all_events();
                                let (i, o, cc, cr, credits) = ctx.final_usage();
                                if let Some(message) = ctx.tool_json_error_message() {
                                    hook.record(credential_id, i, o, cc, cr, credits, "error");
                                    tracer.set_usage(i, o, cc, cr);
                                    tracer.finalize("error", Some(outcome::BAD_REQUEST), Some(&message), None);
                                } else {
                                    hook.record(credential_id, i, o, cc, cr, credits, "success");
                                    cache_plan.commit_success();
                                    tracer.set_usage(i, o, cc, cr);
                                    tracer.finalize("success", None, None, None);
                                }
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, first_upstream_chunk_seen, hook, credential_id, tracer, sent_bytes, cache_plan)));
                            }
                        }
                    }
                }
            }
        },
    )
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bedrock_client_validation_errors_map_to_400() {
        // 客户端校验错误必须映射为 400（而非 5xx），否则会被 provider 当作上游
        // 瞬态错误触发冷却，放大成 503 风暴。识别逻辑集中在 endpoint 层。
        for needle in [
            // 精确 reason（provider 错误串里嵌着上游 body）
            "非流式 API 请求失败: 500 {\"reason\":\"TOOL_USE_RESULT_MISMATCH\"}",
            // message 级特异短语（纯文本报文）
            "Expected toolResult blocks but found none",
        ] {
            let resp = map_provider_error(anyhow::anyhow!(needle.to_string()));
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "错误串 `{needle}` 应映射为 400"
            );
        }
    }

    #[test]
    fn generic_upstream_error_still_maps_to_502() {
        // 回归：普通上游错误不应被新分支误伤，仍应是 502 BAD_GATEWAY。
        let resp = map_provider_error(anyhow::anyhow!("connection reset by peer"));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        // 回归：宽泛的 ValidationException 不再被当作客户端校验错误而误判为 400，
        // 仍按上游错误走 502（避免把可重试故障误杀）。
        let resp = map_provider_error(anyhow::anyhow!(
            "ValidationException: transient backend issue".to_string()
        ));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn available_models_include_opus_4_7_variants() {
        let models = available_models();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();

        assert!(ids.contains(&"claude-opus-4-7"));
        assert!(ids.contains(&"claude-opus-4-7-thinking"));
    }

    #[test]
    fn count_image_budget_handles_empty() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": []
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.total_b64_bytes, 0);
        assert_eq!(stats.largest_b64_bytes, 0);
    }

    #[test]
    fn count_image_budget_counts_inline_base64() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA1111"}},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBBBBBBBB"}},
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                ]
            }]
        }"#).unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_b64_bytes, 18);
        assert_eq!(stats.largest_b64_bytes, 10);
    }

    #[test]
    fn count_image_budget_includes_tool_result_images() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "u1", "content": [
                        {"type": "text", "text": "screenshot"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAABBBBCCCC"}}
                    ]}
                ]
            }]
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 1);
        assert_eq!(stats.total_b64_bytes, 12);
        assert_eq!(stats.largest_b64_bytes, 12);
    }

    #[test]
    fn count_image_budget_skips_url_only_images() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                ]
            }]
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 0);
    }

    #[test]
    fn request_image_budget_limits_reject_count_and_total() {
        let limits = RequestImageLimits {
            max_count: 1,
            max_total_base64_bytes: 12,
        };
        assert!(limits.validate(2, 8).is_err());
        assert!(limits.validate(1, 13).is_err());
        assert!(limits.validate(1, 12).is_ok());
    }

    #[test]
    fn available_models_include_4_8_variants() {
        let models = available_models();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();

        assert!(ids.contains(&"claude-opus-4-8"));
        assert!(ids.contains(&"claude-opus-4-8-thinking"));
        assert!(!ids.contains(&"claude-sonnet-4-8"));
        assert!(!ids.contains(&"claude-sonnet-4-8-thinking"));
    }

    #[test]
    fn thinking_suffix_preserves_explicit_opus_4_8_effort() {
        let mut req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-8-thinking",
            "max_tokens": 100,
            "thinking": {"type": "adaptive", "budget_tokens": 20000},
            "output_config": {"effort": "xhigh"},
            "messages": [{"role": "user", "content": "hi"}]
        }"#,
        )
        .unwrap();

        override_thinking_from_model_name(&mut req);

        assert_eq!(req.thinking.unwrap().thinking_type, "adaptive");
        assert_eq!(req.output_config.unwrap().effort, "xhigh");
    }

    #[test]
    fn thinking_suffix_can_set_opus_4_8_effort() {
        let mut req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-8-thinking-max",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}]
        }"#,
        )
        .unwrap();

        override_thinking_from_model_name(&mut req);

        assert_eq!(req.thinking.unwrap().thinking_type, "adaptive");
        assert_eq!(req.output_config.unwrap().effort, "max");
    }

    #[test]
    fn thinking_suffix_can_set_sonnet_4_6_effort() {
        let mut req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-sonnet-4-6-thinking-max",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}]
        }"#,
        )
        .unwrap();

        override_thinking_from_model_name(&mut req);

        assert_eq!(req.thinking.unwrap().thinking_type, "adaptive");
        assert_eq!(req.output_config.unwrap().effort, "max");
    }
}
