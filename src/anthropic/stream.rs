//! 流式响应处理模块
//!
//! 实现 Kiro → Anthropic 流式响应转换和 SSE 状态管理

use std::collections::HashMap;

use serde_json::json;
use uuid::Uuid;

use crate::kiro::model::events::Event;

use super::converter::restore_tool_use_for_client;

/// thinking 块的 signature 占位字符串
///
/// Anthropic Messages API 协议规定 thinking 模式下，assistant 消息的
/// `{type:"thinking", ...}` 块必须带 `signature` 字段并在下一轮原样回传，
/// 否则 SDK / 服务端会拒绝请求并报：
/// `The content[].thinking in the thinking mode must be passed back to the API`。
///
/// 上游 Kiro 可能下发自己的 reasoning signature，但它不是 Anthropic thinking
/// signature；向下游透传会让 CCH 等客户端误按 Anthropic protobuf 解码出 Kiro
/// 内部模型代号。因此 kiro.rs 在 thinking 块结束时只插入一个非空占位字符串，
/// 满足客户端本地校验即可。
/// converter 在解析 assistant 消息回传 Kiro 时只读 `block.thinking`，不读
/// signature，因此该占位字符串只在客户端 ↔ kiro.rs 之间存在，不会影响转发。
pub(super) const THINKING_SIGNATURE_PLACEHOLDER: &str = "kiro-rs-thinking-signature";

const TOOL_USE_XML_PREFIX: &str = "<tool_use";
const TOOL_USE_XML_CLOSE: &str = "</tool_use>";

#[derive(Debug, Default)]
struct ToolUseXmlLeakFilter {
    buffer: String,
    stripping: bool,
}

impl ToolUseXmlLeakFilter {
    fn filter(&mut self, content: &str) -> String {
        self.buffer.push_str(content);
        let mut out = String::with_capacity(self.buffer.len());
        let mut rest = self.buffer.as_str();

        loop {
            if self.stripping {
                if let Some(close_start) = rest.find(TOOL_USE_XML_CLOSE) {
                    rest = &rest[close_start + TOOL_USE_XML_CLOSE.len()..];
                    self.stripping = false;
                    continue;
                }
                self.buffer.clear();
                return out;
            }

            let Some(start) = rest.find(TOOL_USE_XML_PREFIX) else {
                let keep = longest_tool_use_prefix_suffix(rest);
                let emit_len = rest.len().saturating_sub(keep);
                out.push_str(&rest[..emit_len]);
                self.buffer = rest[emit_len..].to_string();
                return out;
            };

            out.push_str(&rest[..start]);
            let after_start = &rest[start..];
            let Some(open_end) = after_start.find('>') else {
                if is_potential_tool_use_tag_start(after_start) {
                    self.stripping = true;
                    self.buffer.clear();
                    return out;
                }
                out.push_str(&after_start[..TOOL_USE_XML_PREFIX.len()]);
                rest = &after_start[TOOL_USE_XML_PREFIX.len()..];
                continue;
            };

            let tag_head = &after_start[..open_end];
            if !tag_head
                .get(TOOL_USE_XML_PREFIX.len()..)
                .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace))
            {
                out.push_str(&after_start[..TOOL_USE_XML_PREFIX.len()]);
                rest = &after_start[TOOL_USE_XML_PREFIX.len()..];
                continue;
            }

            let after_open = &after_start[open_end + 1..];
            if let Some(close_start) = after_open.find(TOOL_USE_XML_CLOSE) {
                rest = &after_open[close_start + TOOL_USE_XML_CLOSE.len()..];
            } else {
                self.stripping = true;
                self.buffer.clear();
                return out;
            }
        }
    }

    fn finish(&mut self) -> String {
        self.stripping = false;
        let remaining = std::mem::take(&mut self.buffer);
        if remaining.is_empty() {
            String::new()
        } else {
            crate::kiro::model::events::strip_tool_use_xml_leaks(&remaining)
        }
    }
}

fn is_potential_tool_use_tag_start(s: &str) -> bool {
    TOOL_USE_XML_PREFIX.starts_with(s)
        || s.get(TOOL_USE_XML_PREFIX.len()..)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace))
}

fn longest_tool_use_prefix_suffix(s: &str) -> usize {
    let max = s.len().min(TOOL_USE_XML_PREFIX.len().saturating_sub(1));
    for len in (1..=max).rev() {
        if s.is_char_boundary(s.len() - len) && TOOL_USE_XML_PREFIX.starts_with(&s[s.len() - len..])
        {
            return len;
        }
    }
    0
}

/// 找到小于等于目标位置的最近有效UTF-8字符边界
///
/// UTF-8字符可能占用1-4个字节，直接按字节位置切片可能会切在多字节字符中间导致panic。
/// 这个函数从目标位置向前搜索，找到最近的有效字符边界。
fn find_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    if target == 0 {
        return 0;
    }
    // 从目标位置向前搜索有效的字符边界
    let mut pos = target;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// 需要跳过的包裹字符
///
/// 当 thinking 标签被这些字符包裹时，认为是在引用标签而非真正的标签：
/// - 反引号 (`)：行内代码
/// - 双引号 (")：字符串
/// - 单引号 (')：字符串
const QUOTE_CHARS: &[u8] = &[
    b'`', b'"', b'\'', b'\\', b'#', b'!', b'@', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'-',
    b'_', b'=', b'+', b'[', b']', b'{', b'}', b';', b':', b'<', b'>', b',', b'.', b'?', b'/',
];

/// 检查指定位置的字符是否是引用字符
fn is_quote_char(buffer: &str, pos: usize) -> bool {
    buffer
        .as_bytes()
        .get(pos)
        .map(|c| QUOTE_CHARS.contains(c))
        .unwrap_or(false)
}

/// 查找真正的 thinking 结束标签（不被引用字符包裹，且后面有双换行符）
///
/// 当模型在思考过程中提到 `</thinking>` 时，通常会用反引号、引号等包裹，
/// 或者在同一行有其他内容（如"关于 </thinking> 标签"）。
/// 这个函数会跳过这些情况，只返回真正的结束标签位置。
///
/// 跳过的情况：
/// - 被引用字符包裹（反引号、引号等）
/// - 后面没有双换行符（真正的结束标签后面会有 `\n\n`）
/// - 标签在缓冲区末尾（流式处理时需要等待更多内容）
///
/// # 参数
/// - `buffer`: 要搜索的字符串
///
/// # 返回值
/// - `Some(pos)`: 真正的结束标签的起始位置
/// - `None`: 没有找到真正的结束标签
fn find_real_thinking_end_tag_with_boundary(buffer: &str) -> Option<(usize, usize)> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        // 如果被引用字符包裹，跳过
        if has_quote_before || has_quote_after {
            search_start = absolute_pos + 1;
            continue;
        }

        // 检查后面的内容
        let after_content = &buffer[after_pos..];

        if after_content.is_empty() {
            return Some((absolute_pos, after_pos));
        }
        if after_content.trim().is_empty() {
            return Some((absolute_pos, buffer.len()));
        }
        if after_content.starts_with("\r\n\r\n") {
            return Some((absolute_pos, after_pos + 4));
        }
        if after_content.starts_with("\n\n") {
            return Some((absolute_pos, after_pos + 2));
        }
        if after_content.starts_with("\r\n") {
            return Some((absolute_pos, after_pos + 2));
        }
        if after_content.starts_with('\n') {
            return Some((absolute_pos, after_pos + 1));
        }

        // 标签后直接接正文也应关闭 thinking，并把后续正文作为普通 text 继续处理。
        return Some((absolute_pos, after_pos));
    }

    None
}

#[cfg(test)]
fn find_real_thinking_end_tag(buffer: &str) -> Option<usize> {
    find_real_thinking_end_tag_with_boundary(buffer).map(|(pos, _)| pos)
}

/// 查找缓冲区末尾的 thinking 结束标签（允许末尾只有空白字符）
///
/// 用于“边界事件”场景：例如 thinking 结束后立刻进入 tool_use，或流结束，
/// 此时 `</thinking>` 后面可能没有 `\n\n`，但结束标签依然应被识别并过滤。
///
/// 约束：只有当 `</thinking>` 之后全部都是空白字符时才认为是结束标签，
/// 以避免在 thinking 内容中提到 `</thinking>`（非结束标签）时误判。
fn find_real_thinking_end_tag_at_buffer_end(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        if has_quote_before || has_quote_after {
            search_start = absolute_pos + 1;
            continue;
        }

        // 只有当标签后面全部是空白字符时才认定为结束标签
        if buffer[after_pos..].trim().is_empty() {
            return Some(absolute_pos);
        }

        search_start = absolute_pos + 1;
    }

    None
}

/// 查找真正的 thinking 开始标签（不被引用字符包裹）
///
/// 与 `find_real_thinking_end_tag` 类似，跳过被引用字符包裹的开始标签。
fn find_real_thinking_start_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "<thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        // 如果不被引用字符包裹，则是真正的开始标签
        if !has_quote_before && !has_quote_after {
            return Some(absolute_pos);
        }

        // 继续搜索下一个匹配
        search_start = absolute_pos + 1;
    }

    None
}

/// 从完整文本中提取 thinking 块（用于非流式响应）
///
/// 使用与流式处理相同的标签检测逻辑（引用字符过滤），确保一致性。
/// 非流式场景下文本已完整，无需处理跨 chunk 分割问题。
///
/// # 返回值
/// - `(Some(thinking_content), remaining_text)` — 检测到有效 thinking 块
/// - `(None, original_text)` — 未检测到，原样返回
pub(crate) fn extract_thinking_from_complete_text(text: &str) -> (Option<String>, String) {
    let start_pos = match find_real_thinking_start_tag(text) {
        Some(pos) => pos,
        None => return (None, text.to_string()),
    };

    let before = &text[..start_pos];
    let after_open = &text[start_pos + "<thinking>".len()..];

    // 查找结束标签：优先匹配带 \n\n 后缀的，退而使用末尾匹配
    let (thinking_raw, text_after) =
        if let Some((end_pos, after_tag)) = find_real_thinking_end_tag_with_boundary(after_open) {
            (&after_open[..end_pos], &after_open[after_tag..])
        } else if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(after_open) {
            let after_tag = end_pos + "</thinking>".len();
            (&after_open[..end_pos], after_open[after_tag..].trim_start())
        } else {
            // 找不到有效的结束标签，不做提取
            return (None, text.to_string());
        };

    // 剥离开头的换行符（与流式处理一致：模型输出 <thinking>\n）
    let thinking_content = thinking_raw.strip_prefix('\n').unwrap_or(thinking_raw);

    // 组装剩余文本：跳过纯空白的 before 部分
    let mut remaining = String::new();
    if !before.trim().is_empty() {
        remaining.push_str(before);
    }
    remaining.push_str(text_after);

    if thinking_content.is_empty() {
        (None, remaining)
    } else {
        (Some(thinking_content.to_string()), remaining)
    }
}

/// SSE 事件
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

impl SseEvent {
    pub fn new(event: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    /// 格式化为 SSE 字符串
    pub fn to_sse_string(&self) -> String {
        format!(
            "event: {}\ndata: {}\n\n",
            self.event,
            serde_json::to_string(&self.data).unwrap_or_default()
        )
    }
}

/// 内容块状态
#[derive(Debug, Clone)]
struct BlockState {
    block_type: String,
    started: bool,
    stopped: bool,
}

#[derive(Debug, Clone)]
pub struct CompletedToolUse {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone)]
pub enum ToolJsonAccumulatorError {
    InvalidJson {
        tool_use_id: String,
        name: String,
        message: String,
    },
    IncompleteJson {
        tool_use_id: String,
        name: String,
        bytes: usize,
    },
}

impl ToolJsonAccumulatorError {
    pub fn error_type(&self) -> &'static str {
        "upstream_tool_json_error"
    }

    pub fn message(&self) -> String {
        match self {
            Self::InvalidJson {
                tool_use_id,
                name,
                message,
                ..
            } => format!(
                "Upstream returned invalid JSON for tool_use {} ({}): {}",
                tool_use_id, name, message
            ),
            Self::IncompleteJson {
                tool_use_id,
                name,
                bytes,
            } => format!(
                "Upstream ended before completing tool_use {} ({}) JSON input; buffered {} bytes. The tool call was not forwarded to the client.",
                tool_use_id, name, bytes
            ),
        }
    }
}

impl std::fmt::Display for ToolJsonAccumulatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ToolJsonAccumulatorError {}

#[derive(Debug, Default)]
pub struct ToolJsonAccumulator {
    buffers: HashMap<String, (String, String)>,
}

impl ToolJsonAccumulator {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
        }
    }

    pub fn push(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
        tool_name_map: &HashMap<String, String>,
    ) -> Result<Option<CompletedToolUse>, ToolJsonAccumulatorError> {
        let entry = self
            .buffers
            .entry(tool_use.tool_use_id.clone())
            .or_insert_with(|| (tool_use.name.clone(), String::new()));
        if entry.0.is_empty() {
            entry.0 = tool_use.name.clone();
        }
        entry.1.push_str(&tool_use.input);

        if !tool_use.stop {
            return Ok(None);
        }

        let (kiro_name, input_json) = self
            .buffers
            .remove(&tool_use.tool_use_id)
            .unwrap_or_else(|| (tool_use.name.clone(), tool_use.input.clone()));
        let input = if input_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str::<serde_json::Value>(&input_json).map_err(|e| {
                ToolJsonAccumulatorError::InvalidJson {
                    tool_use_id: tool_use.tool_use_id.clone(),
                    name: kiro_name.clone(),
                    message: e.to_string(),
                }
            })?
        };

        let (name, input) = restore_tool_use_for_client(&kiro_name, input, tool_name_map);
        Ok(Some(CompletedToolUse {
            id: tool_use.tool_use_id.clone(),
            name,
            input,
        }))
    }

    /// 流结束时处理仍未闭合（未收到 stop=true）的工具缓冲。
    ///
    /// 区分两种 dangling buffer：
    /// - **空缓冲**（input 去空白后为空）：这类工具（如 EnterPlanMode / ExitPlanMode / Agent 等
    ///   无参数工具）上游可能只发了 toolUse 事件而未补 stop 分片，但其参数本就该是 `{}`。
    ///   与 [`Self::push`] 对"收到 stop 的空输入"的处理保持一致——当作合法工具发出，**不报错**。
    /// - **非空但 JSON 不完整**：真正被截断的半截 JSON（如大 Write/Edit 参数被上游截断）。
    ///   仍返回 [`ToolJsonAccumulatorError::IncompleteJson`]，保留对客户端的防护，避免把半截参数
    ///   泄漏给 Claude Code 触发工具执行失败 / 会话卡死。
    ///
    /// 返回成功"补发"的空参数工具（可能多个）；若存在非空残缺缓冲（半截 JSON），
    /// 取其中最长的一个作为代表返回 Err。
    pub fn finish(
        &mut self,
        tool_name_map: &HashMap<String, String>,
    ) -> Result<Vec<CompletedToolUse>, ToolJsonAccumulatorError> {
        // 先弹出全部空缓冲，按合法 `{}` 工具补发（与 push 的空输入分支同构）。
        let empty_ids: Vec<String> = self
            .buffers
            .iter()
            .filter(|(_, (_, input))| input.trim().is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        let mut completed = Vec::with_capacity(empty_ids.len());
        for id in empty_ids {
            if let Some((kiro_name, _)) = self.buffers.remove(&id) {
                let (name, input) =
                    restore_tool_use_for_client(&kiro_name, serde_json::json!({}), tool_name_map);
                completed.push(CompletedToolUse { id, name, input });
            }
        }

        // 剩下的都是非空但未闭合的缓冲：真正的半截 JSON，仍按残缺报错（取最长的一个作代表）。
        if let Some((tool_use_id, (name, input))) = self
            .buffers
            .iter()
            .max_by_key(|(_, (_, input))| input.len())
            .map(|(id, (name, input))| (id.clone(), (name.clone(), input.clone())))
        {
            self.buffers.remove(&tool_use_id);
            return Err(ToolJsonAccumulatorError::IncompleteJson {
                tool_use_id,
                name,
                bytes: input.len(),
            });
        }
        Ok(completed)
    }
}

impl BlockState {
    fn new(block_type: impl Into<String>) -> Self {
        Self {
            block_type: block_type.into(),
            started: false,
            stopped: false,
        }
    }
}

/// SSE 状态管理器
///
/// 确保 SSE 事件序列符合 Claude API 规范：
/// 1. message_start 只能出现一次
/// 2. content_block 必须先 start 再 delta 再 stop
/// 3. message_delta 只能出现一次，且在所有 content_block_stop 之后
/// 4. message_stop 在最后
#[derive(Debug)]
pub struct SseStateManager {
    /// message_start 是否已发送
    message_started: bool,
    /// message_delta 是否已发送
    message_delta_sent: bool,
    /// 活跃的内容块状态
    active_blocks: HashMap<i32, BlockState>,
    /// 消息是否已结束
    message_ended: bool,
    /// 下一个块索引
    next_block_index: i32,
    /// 当前 stop_reason
    stop_reason: Option<String>,
    /// 是否有工具调用
    has_tool_use: bool,
}

impl Default for SseStateManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SseStateManager {
    pub fn new() -> Self {
        Self {
            message_started: false,
            message_delta_sent: false,
            active_blocks: HashMap::new(),
            message_ended: false,
            next_block_index: 0,
            stop_reason: None,
            has_tool_use: false,
        }
    }

    /// 判断指定块是否处于可接收 delta 的打开状态
    fn is_block_open_of_type(&self, index: i32, expected_type: &str) -> bool {
        self.active_blocks
            .get(&index)
            .is_some_and(|b| b.started && !b.stopped && b.block_type == expected_type)
    }

    /// 获取下一个块索引
    pub fn next_block_index(&mut self) -> i32 {
        let index = self.next_block_index;
        self.next_block_index += 1;
        index
    }

    /// 记录工具调用
    pub fn set_has_tool_use(&mut self, has: bool) {
        self.has_tool_use = has;
    }

    /// 设置 stop_reason
    pub fn set_stop_reason(&mut self, reason: impl Into<String>) {
        self.stop_reason = Some(reason.into());
    }

    /// 检查是否存在非 thinking 类型的内容块（如 text 或 tool_use）
    fn has_non_thinking_blocks(&self) -> bool {
        self.active_blocks
            .values()
            .any(|b| b.block_type != "thinking")
    }

    /// 获取最终的 stop_reason
    pub fn get_stop_reason(&self) -> String {
        if let Some(ref reason) = self.stop_reason {
            reason.clone()
        } else if self.has_tool_use {
            "tool_use".to_string()
        } else {
            "end_turn".to_string()
        }
    }

    /// 处理 message_start 事件
    pub fn handle_message_start(&mut self, event: serde_json::Value) -> Option<SseEvent> {
        if self.message_started {
            tracing::debug!("跳过重复的 message_start 事件");
            return None;
        }
        self.message_started = true;
        Some(SseEvent::new("message_start", event))
    }

    /// 处理 content_block_start 事件
    pub fn handle_content_block_start(
        &mut self,
        index: i32,
        block_type: &str,
        data: serde_json::Value,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果是 tool_use 块，先关闭之前的文本块
        if block_type == "tool_use" {
            self.has_tool_use = true;
            for (block_index, block) in self.active_blocks.iter_mut() {
                if block.block_type == "text" && block.started && !block.stopped {
                    // 自动发送 content_block_stop 关闭文本块
                    events.push(SseEvent::new(
                        "content_block_stop",
                        json!({
                            "type": "content_block_stop",
                            "index": block_index
                        }),
                    ));
                    block.stopped = true;
                }
            }
        }

        // 检查块是否已存在
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.started {
                tracing::debug!("块 {} 已启动，跳过重复的 content_block_start", index);
                return events;
            }
            block.started = true;
        } else {
            let mut block = BlockState::new(block_type);
            block.started = true;
            self.active_blocks.insert(index, block);
        }

        events.push(SseEvent::new("content_block_start", data));
        events
    }

    /// 处理 content_block_delta 事件
    pub fn handle_content_block_delta(
        &mut self,
        index: i32,
        data: serde_json::Value,
    ) -> Option<SseEvent> {
        // 确保块已启动
        if let Some(block) = self.active_blocks.get(&index) {
            if !block.started || block.stopped {
                tracing::warn!(
                    "块 {} 状态异常: started={}, stopped={}",
                    index,
                    block.started,
                    block.stopped
                );
                return None;
            }
        } else {
            // 块不存在，可能需要先创建
            tracing::warn!("收到未知块 {} 的 delta 事件", index);
            return None;
        }

        Some(SseEvent::new("content_block_delta", data))
    }

    /// 处理 content_block_stop 事件
    pub fn handle_content_block_stop(&mut self, index: i32) -> Option<SseEvent> {
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.stopped {
                tracing::debug!("块 {} 已停止，跳过重复的 content_block_stop", index);
                return None;
            }
            block.stopped = true;
            return Some(SseEvent::new(
                "content_block_stop",
                json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
        }
        None
    }

    /// 生成最终事件序列
    pub fn generate_final_events(
        &mut self,
        input_tokens: i32,
        output_tokens: i32,
        cache_creation_input_tokens: i32,
        cache_read_input_tokens: i32,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 关闭所有未关闭的块
        for (index, block) in self.active_blocks.iter_mut() {
            if block.started && !block.stopped {
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop",
                        "index": index
                    }),
                ));
                block.stopped = true;
            }
        }

        // 发送 message_delta
        if !self.message_delta_sent {
            self.message_delta_sent = true;
            events.push(SseEvent::new(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": self.get_stop_reason(),
                        "stop_sequence": null
                    },
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens,
                        "cache_creation_input_tokens": cache_creation_input_tokens,
                        "cache_read_input_tokens": cache_read_input_tokens
                    }
                }),
            ));
        }

        // 发送 message_stop
        if !self.message_ended {
            self.message_ended = true;
            events.push(SseEvent::new(
                "message_stop",
                json!({ "type": "message_stop" }),
            ));
        }

        events
    }
}

use super::converter::get_context_window_size;

/// 流处理上下文
pub struct StreamContext {
    /// SSE 状态管理器
    pub state_manager: SseStateManager,
    /// 请求的模型名称
    pub model: String,
    /// 消息 ID
    pub message_id: String,
    /// 输入 tokens（估算值）
    pub input_tokens: i32,
    /// 从 contextUsageEvent 计算的实际输入 tokens
    pub context_input_tokens: Option<i32>,
    /// 输出 tokens 累计
    pub output_tokens: i32,
    /// 工具块索引映射 (tool_id -> block_index)
    pub tool_block_indices: HashMap<String, i32>,
    /// 工具名称反向映射（短名称 → 原始名称），用于响应时还原
    pub tool_name_map: HashMap<String, String>,
    /// thinking 是否启用
    pub thinking_enabled: bool,
    /// thinking 内容缓冲区
    pub thinking_buffer: String,
    /// 是否在 thinking 块内
    pub in_thinking_block: bool,
    /// thinking 块是否已提取完成
    pub thinking_extracted: bool,
    /// thinking 块索引
    pub thinking_block_index: Option<i32>,
    /// 是否存在由 reasoningContentEvent 打开的 thinking 块
    pub reasoning_block_open: bool,
    /// 文本块索引（thinking 启用时动态分配）
    pub text_block_index: Option<i32>,
    /// 是否需要剥离 thinking 内容开头的换行符
    /// 模型输出 `<thinking>\n` 时，`\n` 可能与标签在同一 chunk 或下一 chunk
    strip_thinking_leading_newline: bool,
    /// thinking 结束后，下一段正文开头的协议分隔空白需要剥离一次。
    strip_text_after_thinking_boundary: bool,
    /// 中转层 CacheMeter 的缓存覆盖情况（estimate 口径）。最终上报时按真实 total
    /// 做互斥分摊：`input + cache_creation + cache_read == total`。
    pub cache_usage: super::cache_metering::CacheUsage,
    /// meteringEvent 上报的 credit 计费量（上游真实下发）
    pub credits: f64,
    /// Kiro toolUseEvent.input JSON 聚合器
    pub tool_json_accumulator: ToolJsonAccumulator,
    /// 上游工具 JSON 解析错误
    pub tool_json_error: Option<ToolJsonAccumulatorError>,
    tool_use_xml_filter: ToolUseXmlLeakFilter,
}

impl StreamContext {
    /// 解析最终上报口径的 `(input_tokens, cache_creation, cache_read)`。
    pub fn resolved_usage(&self) -> (i32, i32, i32) {
        let total_real = self.context_input_tokens.unwrap_or(self.input_tokens);
        self.cache_usage.split_against_total(total_real)
    }

    /// 创建 StreamContext
    pub fn new_with_thinking(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        Self {
            state_manager: SseStateManager::new(),
            model: model.into(),
            message_id: format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
            input_tokens,
            context_input_tokens: None,
            output_tokens: 0,
            tool_block_indices: HashMap::new(),
            tool_name_map,
            thinking_enabled,
            thinking_buffer: String::new(),
            in_thinking_block: false,
            thinking_extracted: false,
            thinking_block_index: None,
            reasoning_block_open: false,
            text_block_index: None,
            strip_thinking_leading_newline: false,
            strip_text_after_thinking_boundary: false,
            cache_usage: super::cache_metering::CacheUsage::default(),
            credits: 0.0,
            tool_json_accumulator: ToolJsonAccumulator::new(),
            tool_json_error: None,
            tool_use_xml_filter: ToolUseXmlLeakFilter::default(),
        }
    }

    /// 生成 message_start 事件
    pub fn create_message_start_event(&self) -> serde_json::Value {
        let (input_tokens, cache_creation_input_tokens, cache_read_input_tokens) =
            self.cache_usage.split_against_total(self.input_tokens);

        json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": 1,
                    "cache_creation_input_tokens": cache_creation_input_tokens,
                    "cache_read_input_tokens": cache_read_input_tokens
                }
            }
        })
    }

    /// 生成初始事件序列 (message_start + 文本块 start)
    ///
    /// 当 thinking 启用时，不在初始化时创建文本块，而是等到实际收到内容时再创建。
    /// 这样可以确保 thinking 块（索引 0）在文本块（索引 1）之前。
    pub fn generate_initial_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // message_start
        let msg_start = self.create_message_start_event();
        if let Some(event) = self.state_manager.handle_message_start(msg_start) {
            events.push(event);
        }

        // 如果启用了 thinking，不在这里创建文本块
        // thinking 块和文本块会在 process_content_with_thinking 中按正确顺序创建
        if self.thinking_enabled {
            return events;
        }

        // 创建初始文本块（仅在未启用 thinking 时）
        let text_block_index = self.state_manager.next_block_index();
        self.text_block_index = Some(text_block_index);
        let text_block_events = self.state_manager.handle_content_block_start(
            text_block_index,
            "text",
            json!({
                "type": "content_block_start",
                "index": text_block_index,
                "content_block": {
                    "type": "text",
                    "text": ""
                }
            }),
        );
        events.extend(text_block_events);

        events
    }

    /// 处理 Kiro 事件并转换为 Anthropic SSE 事件
    pub fn process_kiro_event(&mut self, event: &Event) -> Vec<SseEvent> {
        match event {
            Event::AssistantResponse(resp) => {
                let mut events = self.close_reasoning_if_open();
                events.extend(self.process_assistant_response(&resp.content));
                events
            }
            Event::Code(resp) => {
                let mut events = self.close_reasoning_if_open();
                events.extend(self.process_assistant_response(&resp.content));
                events
            }
            Event::ToolUse(tool_use) => {
                let mut events = self.close_reasoning_if_open();
                events.extend(self.process_tool_use(tool_use));
                events
            }
            Event::ReasoningContent(reasoning) => self.process_reasoning_content(reasoning),
            Event::ContextUsage(context_usage) => {
                // 从上下文使用百分比计算实际的 input_tokens
                let window_size = get_context_window_size(&self.model);
                let actual_input_tokens =
                    (context_usage.context_usage_percentage * (window_size as f64) / 100.0) as i32;
                self.context_input_tokens = Some(actual_input_tokens);
                // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                if context_usage.context_usage_percentage >= 100.0 {
                    self.state_manager
                        .set_stop_reason("model_context_window_exceeded");
                }
                tracing::debug!(
                    "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                    context_usage.context_usage_percentage,
                    actual_input_tokens
                );
                Vec::new()
            }
            Event::Metering(metering) => {
                // 上游 meteringEvent 只下发 credit；token / cache 字段不存在。
                self.credits += metering.usage;
                tracing::debug!("metering credits +{:.6}", metering.usage);
                Vec::new()
            }
            Event::Error {
                error_code,
                error_message,
            } => {
                tracing::error!("收到错误事件: {} - {}", error_code, error_message);
                Vec::new()
            }
            Event::Exception {
                exception_type,
                message,
            } => {
                // 处理 ContentLengthExceededException
                if exception_type == "ContentLengthExceededException" {
                    self.state_manager.set_stop_reason("max_tokens");
                }
                tracing::warn!("收到异常事件: {} - {}", exception_type, message);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// 处理助手响应事件
    fn process_assistant_response(&mut self, content: &str) -> Vec<SseEvent> {
        let content = self.tool_use_xml_filter.filter(content);
        if content.is_empty() {
            return Vec::new();
        }

        // 估算 tokens
        self.output_tokens += estimate_tokens(&content);

        // 如果启用了thinking，需要处理thinking块
        if self.thinking_enabled {
            return self.process_content_with_thinking(&content);
        }

        // 非 thinking 模式同样复用统一的 text_delta 发送逻辑，
        // 以便在 tool_use 自动关闭文本块后能够自愈重建新的文本块，避免“吞字”。
        self.create_text_delta_events(&content)
    }

    /// 处理包含thinking块的内容
    fn process_content_with_thinking(&mut self, content: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 将内容添加到缓冲区进行处理
        self.thinking_buffer.push_str(content);

        loop {
            if !self.in_thinking_block && !self.thinking_extracted {
                // 查找 <thinking> 开始标签（跳过被反引号包裹的）
                if let Some(start_pos) = find_real_thinking_start_tag(&self.thinking_buffer) {
                    // 发送 <thinking> 之前的内容作为 text_delta
                    // 注意：如果前面只是空白字符（如 adaptive 模式返回的 \n\n），则跳过，
                    // 避免在 thinking 块之前产生无意义的 text 块导致客户端解析失败
                    let before_thinking = self.thinking_buffer[..start_pos].to_string();
                    if !before_thinking.is_empty() && !before_thinking.trim().is_empty() {
                        events.extend(self.create_text_delta_events(&before_thinking));
                    }

                    // 进入 thinking 块
                    self.in_thinking_block = true;
                    self.strip_thinking_leading_newline = true;
                    self.thinking_buffer =
                        self.thinking_buffer[start_pos + "<thinking>".len()..].to_string();

                    // 创建 thinking 块的 content_block_start 事件
                    let thinking_index = self.state_manager.next_block_index();
                    self.thinking_block_index = Some(thinking_index);
                    let start_events = self.state_manager.handle_content_block_start(
                        thinking_index,
                        "thinking",
                        json!({
                            "type": "content_block_start",
                            "index": thinking_index,
                            "content_block": {
                                "type": "thinking",
                                "thinking": ""
                            }
                        }),
                    );
                    events.extend(start_events);
                } else {
                    // 没有找到 <thinking>，检查是否可能是部分标签
                    // 保留可能是部分标签的内容
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub("<thinking>".len());
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        // 如果 thinking 尚未提取，且安全内容只是空白字符，
                        // 则不发送为 text_delta，继续保留在缓冲区等待更多内容。
                        // 这避免了 4.6 模型中 <thinking> 标签跨事件分割时，
                        // 前导空白（如 "\n\n"）被错误地创建为 text 块，
                        // 导致 text 块先于 thinking 块出现的问题。
                        if !safe_content.is_empty() && !safe_content.trim().is_empty() {
                            events.extend(self.create_text_delta_events(&safe_content));
                            self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                        }
                    }
                    break;
                }
            } else if self.in_thinking_block {
                // 剥离 <thinking> 标签后紧跟的换行符（可能跨 chunk）
                if self.strip_thinking_leading_newline {
                    if self.thinking_buffer.starts_with('\n') {
                        self.thinking_buffer = self.thinking_buffer[1..].to_string();
                        self.strip_thinking_leading_newline = false;
                    } else if !self.thinking_buffer.is_empty() {
                        // buffer 非空但不以 \n 开头，不再需要剥离
                        self.strip_thinking_leading_newline = false;
                    }
                    // buffer 为空时保留标志，等待下一个 chunk
                }

                // 在 thinking 块内，查找 </thinking> 结束标签（跳过被反引号包裹的）
                if let Some((end_pos, after_tag)) =
                    find_real_thinking_end_tag_with_boundary(&self.thinking_buffer)
                {
                    // 提取 thinking 内容
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            events.push(
                                self.create_thinking_delta_event(thinking_index, &thinking_content),
                            );
                        }
                    }

                    // 结束 thinking 块
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;
                    self.strip_text_after_thinking_boundary = true;

                    // 发送空的 thinking_delta 事件，然后发送 content_block_stop 事件
                    if let Some(thinking_index) = self.thinking_block_index {
                        // 先发送空的 thinking_delta
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // signature_delta：满足客户端 thinking 模式下的本地校验
                        events.push(self.create_signature_delta_event(thinking_index));
                        // 再发送 content_block_stop
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }

                    self.thinking_buffer = self.thinking_buffer[after_tag..].to_string();
                } else {
                    // 没有找到结束标签，发送当前缓冲区内容作为 thinking_delta。
                    // 保留末尾可能是部分 `</thinking>` 的内容：
                    // 因此保留区必须覆盖 `</thinking>` 的完整长度，
                    // 否则当 `</thinking>` 已在 buffer 但 `\n\n` 尚未到达时，
                    // 标签的前几个字符会被错误地作为 thinking_delta 发出。
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub("</thinking>".len());
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        if !safe_content.is_empty() {
                            if let Some(thinking_index) = self.thinking_block_index {
                                events.push(
                                    self.create_thinking_delta_event(thinking_index, &safe_content),
                                );
                            }
                        }
                        self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                    }
                    break;
                }
            } else {
                // thinking 已提取完成，剩余内容作为 text_delta
                if !self.thinking_buffer.is_empty() {
                    let remaining = if self.strip_text_after_thinking_boundary {
                        let trimmed = self.thinking_buffer.trim_start().to_string();
                        if !trimmed.is_empty() {
                            self.strip_text_after_thinking_boundary = false;
                        }
                        trimmed
                    } else {
                        self.thinking_buffer.clone()
                    };
                    self.thinking_buffer.clear();
                    if !remaining.is_empty() {
                        events.extend(self.create_text_delta_events(&remaining));
                    }
                }
                break;
            }
        }

        events
    }

    /// 创建 text_delta 事件
    ///
    /// 如果文本块尚未创建，会先创建文本块。
    /// 当发生 tool_use 时，状态机会自动关闭当前文本块；后续文本会自动创建新的文本块继续输出。
    ///
    /// 返回值包含可能的 content_block_start 事件和 content_block_delta 事件。
    fn create_text_delta_events(&mut self, text: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果当前 text_block_index 指向的块已经被关闭（例如 tool_use 开始时自动 stop），
        // 则丢弃该索引并创建新的文本块继续输出，避免 delta 被状态机拒绝导致“吞字”。
        if let Some(idx) = self.text_block_index {
            if !self.state_manager.is_block_open_of_type(idx, "text") {
                self.text_block_index = None;
            }
        }

        // 获取或创建文本块索引
        let text_index = if let Some(idx) = self.text_block_index {
            idx
        } else {
            // 文本块尚未创建，需要先创建
            let idx = self.state_manager.next_block_index();
            self.text_block_index = Some(idx);

            // 发送 content_block_start 事件
            let start_events = self.state_manager.handle_content_block_start(
                idx,
                "text",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": "text",
                        "text": ""
                    }
                }),
            );
            events.extend(start_events);
            idx
        };

        // 发送 content_block_delta 事件
        if let Some(delta_event) = self.state_manager.handle_content_block_delta(
            text_index,
            json!({
                "type": "content_block_delta",
                "index": text_index,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ) {
            events.push(delta_event);
        }

        events
    }

    /// 创建 thinking_delta 事件
    fn create_thinking_delta_event(&self, index: i32, thinking: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": thinking
                }
            }),
        )
    }

    /// 创建 signature_delta 事件
    ///
    /// Anthropic 协议下 thinking 块流式结束前必须发一个 signature_delta，
    /// SDK 会把它聚合到 thinking 块的 `signature` 字段。客户端在下一轮把
    /// assistant 消息回传时本地校验 thinking 块必须带非空 signature，否则抛出
    /// `The content[].thinking in the thinking mode must be passed back to the API`。
    ///
    /// 上游 Kiro 的 reasoning signature 不是 Anthropic thinking signature，不能
    /// 透传给下游；否则 CCH 会尝试按 Anthropic protobuf 解码并误识别模型。
    /// 因此这里始终发本地占位字符串。该字段不参与转发回 Kiro 的逻辑。
    fn create_signature_delta_event(&self, index: i32) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "signature_delta",
                    "signature": THINKING_SIGNATURE_PLACEHOLDER,
                }
            }),
        )
    }

    fn close_text_if_open(&mut self) -> Vec<SseEvent> {
        let Some(text_index) = self.text_block_index else {
            return Vec::new();
        };

        if self.state_manager.is_block_open_of_type(text_index, "text")
            && let Some(stop_event) = self.state_manager.handle_content_block_stop(text_index)
        {
            return vec![stop_event];
        }

        Vec::new()
    }

    fn process_reasoning_content(
        &mut self,
        reasoning: &crate::kiro::model::events::ReasoningContentEvent,
    ) -> Vec<SseEvent> {
        if !self.thinking_enabled {
            // Kiro may emit reasoningContentEvent even when the downstream client
            // did not request Anthropic thinking blocks. Treat it as hidden
            // reasoning, not assistant text, otherwise model internals leak into
            // Claude Code's visible response.
            return Vec::new();
        }

        let mut events = Vec::new();

        if let Some(redacted) = reasoning
            .redacted_content
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            events.extend(self.close_reasoning_if_open());
            events.extend(self.close_text_if_open());

            let redacted_index = self.state_manager.next_block_index();
            events.extend(self.state_manager.handle_content_block_start(
                redacted_index,
                "redacted_thinking",
                json!({
                    "type": "content_block_start",
                    "index": redacted_index,
                    "content_block": {
                        "type": "redacted_thinking",
                        "data": redacted
                    }
                }),
            ));
            if let Some(stop_event) = self.state_manager.handle_content_block_stop(redacted_index) {
                events.push(stop_event);
            }

            self.thinking_extracted = true;
            return events;
        }

        if !self.reasoning_block_open {
            events.extend(self.close_text_if_open());

            let thinking_index = match self.thinking_block_index {
                Some(index) if self.state_manager.is_block_open_of_type(index, "thinking") => index,
                _ => self.state_manager.next_block_index(),
            };
            self.thinking_block_index = Some(thinking_index);
            self.reasoning_block_open = true;
            self.in_thinking_block = false;
            self.thinking_extracted = true;

            let start_events = self.state_manager.handle_content_block_start(
                thinking_index,
                "thinking",
                json!({
                    "type": "content_block_start",
                    "index": thinking_index,
                    "content_block": {
                        "type": "thinking",
                        "thinking": ""
                    }
                }),
            );
            events.extend(start_events);
        }

        let text = reasoning.text.as_str();
        if !text.is_empty() {
            self.output_tokens += estimate_tokens(text);
            if let Some(thinking_index) = self.thinking_block_index {
                events.push(self.create_thinking_delta_event(thinking_index, text));
            }
        }

        events
    }

    fn close_reasoning_if_open(&mut self) -> Vec<SseEvent> {
        if !self.reasoning_block_open {
            return Vec::new();
        }

        let mut events = Vec::new();
        if let Some(thinking_index) = self.thinking_block_index {
            events.push(self.create_thinking_delta_event(thinking_index, ""));
            events.push(self.create_signature_delta_event(thinking_index));
            if let Some(stop_event) = self.state_manager.handle_content_block_stop(thinking_index) {
                events.push(stop_event);
            }
        }

        self.reasoning_block_open = false;
        self.thinking_extracted = true;
        events
    }

    /// 处理工具使用事件
    fn process_tool_use(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        self.state_manager.set_has_tool_use(true);

        // tool_use 必须发生在 thinking 结束之后。
        // 但当 `</thinking>` 后面没有 `\n\n`（例如紧跟 tool_use 或流结束）时，
        // thinking 结束标签会滞留在 thinking_buffer，导致后续 flush 时把 `</thinking>` 当作内容输出。
        // 这里在开始 tool_use block 前做一次“边界场景”的结束标签识别与过滤。
        if self.thinking_enabled && self.in_thinking_block {
            if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer) {
                let thinking_content = self.thinking_buffer[..end_pos].to_string();
                if !thinking_content.is_empty() {
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(
                            self.create_thinking_delta_event(thinking_index, &thinking_content),
                        );
                    }
                }

                // 结束 thinking 块
                self.in_thinking_block = false;
                self.thinking_extracted = true;
                self.strip_text_after_thinking_boundary = true;

                if let Some(thinking_index) = self.thinking_block_index {
                    // 先发送空的 thinking_delta
                    events.push(self.create_thinking_delta_event(thinking_index, ""));
                    // signature_delta：满足客户端 thinking 模式下的本地校验
                    events.push(self.create_signature_delta_event(thinking_index));
                    // 再发送 content_block_stop
                    if let Some(stop_event) =
                        self.state_manager.handle_content_block_stop(thinking_index)
                    {
                        events.push(stop_event);
                    }
                }

                // 把结束标签后的内容当作普通文本（通常为空或空白）
                let after_pos = end_pos + "</thinking>".len();
                let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                self.thinking_buffer.clear();
                if !remaining.is_empty() {
                    events.extend(self.create_text_delta_events(&remaining));
                }
            }
        }

        // thinking 模式下，process_content_with_thinking 可能会为了探测 `<thinking>` 而暂存一小段尾部文本。
        // 如果此时直接开始 tool_use，状态机会自动关闭 text block，导致这段"待输出文本"看起来被 tool_use 吞掉。
        // 约束：只在尚未进入 thinking block、且 thinking 尚未被提取时，将缓冲区当作普通文本 flush。
        if self.thinking_enabled
            && !self.in_thinking_block
            && !self.thinking_extracted
            && !self.thinking_buffer.is_empty()
        {
            let buffered = std::mem::take(&mut self.thinking_buffer);
            events.extend(self.create_text_delta_events(&buffered));
        }

        let completed = match self
            .tool_json_accumulator
            .push(tool_use, &self.tool_name_map)
        {
            Ok(Some(completed)) => completed,
            Ok(None) => return events,
            Err(e) => {
                tracing::error!("{}", e);
                self.tool_json_error = Some(e);
                self.state_manager.set_stop_reason("error");
                return events;
            }
        };

        events.extend(self.emit_completed_tool_use(completed));
        events
    }

    /// 把一个已完成（参数完整或空参数）的 tool_use 渲染为 content_block start/delta/stop 事件序列。
    /// 由流式增量路径（[`Self::handle_tool_use`]）与流结束补发路径（`finish` 的空缓冲工具）共用。
    fn emit_completed_tool_use(&mut self, completed: CompletedToolUse) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 获取或分配块索引
        let block_index = if let Some(&idx) = self.tool_block_indices.get(&completed.id) {
            idx
        } else {
            let idx = self.state_manager.next_block_index();
            self.tool_block_indices.insert(completed.id.clone(), idx);
            idx
        };

        // 发送 content_block_start
        let start_events = self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": completed.id,
                    "name": completed.name,
                    "input": {}
                }
            }),
        );
        events.extend(start_events);

        self.output_tokens += estimate_tokens(&completed.input.to_string());

        if let Some(delta_event) = self.state_manager.handle_content_block_delta(
            block_index,
            json!({
                "type": "content_block_delta",
                "index": block_index,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": serde_json::to_string(&completed.input).unwrap_or_else(|_| "{}".to_string())
                }
            }),
        ) {
            events.push(delta_event);
        }

        if let Some(stop_event) = self.state_manager.handle_content_block_stop(block_index) {
            events.push(stop_event);
        }

        events
    }

    /// 生成最终事件序列
    pub fn generate_final_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 流结束时处理未闭合的工具缓冲：空参数工具补发为合法 tool_use，半截 JSON 仍报错。
        if self.tool_json_error.is_none() {
            match self.tool_json_accumulator.finish(&self.tool_name_map) {
                Ok(pending) => {
                    for completed in pending {
                        events.extend(self.emit_completed_tool_use(completed));
                    }
                }
                Err(e) => {
                    tracing::error!("{}", e);
                    self.tool_json_error = Some(e);
                    self.state_manager.set_stop_reason("error");
                }
            }
        }

        let remaining_filtered_text = self.tool_use_xml_filter.finish();
        if !remaining_filtered_text.is_empty() {
            self.output_tokens += estimate_tokens(&remaining_filtered_text);
            if self.thinking_enabled {
                events.extend(self.process_content_with_thinking(&remaining_filtered_text));
            } else {
                events.extend(self.create_text_delta_events(&remaining_filtered_text));
            }
        }

        if let Some(err) = &self.tool_json_error {
            let error_type = err.error_type();
            let error_message = err.message();
            events.extend(self.close_reasoning_if_open());
            let (final_input_tokens, cache_creation, cache_read) = self.resolved_usage();
            events.extend(self.state_manager.generate_final_events(
                final_input_tokens,
                self.output_tokens,
                cache_creation,
                cache_read,
            ));
            events.push(SseEvent::new(
                "error",
                json!({
                    "type": "error",
                    "error": {
                        "type": error_type,
                        "message": error_message
                    }
                }),
            ));
            return events;
        }

        events.extend(self.close_reasoning_if_open());

        // Flush thinking_buffer 中的剩余内容
        if self.thinking_enabled && !self.thinking_buffer.is_empty() {
            if self.in_thinking_block {
                // 末尾可能残留 `</thinking>`（例如紧跟 tool_use 或流结束），需要在 flush 时过滤掉结束标签。
                if let Some(end_pos) =
                    find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer)
                {
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            events.push(
                                self.create_thinking_delta_event(thinking_index, &thinking_content),
                            );
                        }
                    }

                    // 关闭 thinking 块：先发送空的 thinking_delta，再发送 content_block_stop
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // signature_delta：满足客户端 thinking 模式下的本地校验
                        events.push(self.create_signature_delta_event(thinking_index));
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }

                    // 把结束标签后的内容当作普通文本（通常为空或空白）
                    let after_pos = end_pos + "</thinking>".len();
                    let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                    self.thinking_buffer.clear();
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;
                    self.strip_text_after_thinking_boundary = true;
                    if !remaining.is_empty() {
                        events.extend(self.create_text_delta_events(&remaining));
                    }
                } else {
                    // 如果还在 thinking 块内，发送剩余内容作为 thinking_delta
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(
                            self.create_thinking_delta_event(thinking_index, &self.thinking_buffer),
                        );
                    }
                    // 关闭 thinking 块：先发送空的 thinking_delta，再发送 content_block_stop
                    if let Some(thinking_index) = self.thinking_block_index {
                        // 先发送空的 thinking_delta
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // signature_delta：满足客户端 thinking 模式下的本地校验
                        events.push(self.create_signature_delta_event(thinking_index));
                        // 再发送 content_block_stop
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }
                }
            } else {
                // 否则发送剩余内容作为 text_delta
                let buffer_content = if self.strip_text_after_thinking_boundary {
                    let trimmed = self.thinking_buffer.trim_start().to_string();
                    if !trimmed.is_empty() {
                        self.strip_text_after_thinking_boundary = false;
                    }
                    trimmed
                } else {
                    self.thinking_buffer.clone()
                };
                if !buffer_content.is_empty() {
                    events.extend(self.create_text_delta_events(&buffer_content));
                }
            }
            self.thinking_buffer.clear();
        }

        // 如果整个流中只产生了 thinking 块，没有 text 也没有 tool_use，
        // 则设置 stop_reason 为 max_tokens（表示模型耗尽了 token 预算在思考上），
        // 并补发一套完整的 text 事件（内容为一个空格），确保 content 数组中有 text 块
        if self.thinking_enabled
            && self.thinking_block_index.is_some()
            && !self.state_manager.has_non_thinking_blocks()
        {
            self.state_manager.set_stop_reason("max_tokens");
            events.extend(self.create_text_delta_events(" "));
        }

        let (final_input_tokens, cache_creation, cache_read) = self.resolved_usage();

        // 生成最终事件
        events.extend(self.state_manager.generate_final_events(
            final_input_tokens,
            self.output_tokens,
            cache_creation,
            cache_read,
        ));
        events
    }

    pub fn tool_json_error_message(&self) -> Option<String> {
        self.tool_json_error.as_ref().map(|err| err.message())
    }
}

/// 缓冲流处理上下文 - 用于 /cc/v1/messages 流式请求
///
/// 与 `StreamContext` 不同，此上下文会缓冲所有事件直到流结束，
/// 然后用从 `contextUsageEvent` 计算的正确 `input_tokens` 更正 `message_start` 事件。
///
/// 工作流程：
/// 1. 使用 `StreamContext` 正常处理所有 Kiro 事件
/// 2. 把生成的 SSE 事件缓存起来（而不是立即发送）
/// 3. 流结束时，找到 `message_start` 事件并更新其 `input_tokens`
/// 4. 一次性返回所有事件
pub struct BufferedStreamContext {
    /// 内部流处理上下文（复用现有的事件处理逻辑）
    inner: StreamContext,
    /// 缓冲的所有事件（包括 message_start、content_block_start 等）
    event_buffer: Vec<SseEvent>,
    /// 是否已经生成了初始事件
    initial_events_generated: bool,
}

impl BufferedStreamContext {
    /// 创建缓冲流上下文
    pub fn new(
        model: impl Into<String>,
        estimated_input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        let inner = StreamContext::new_with_thinking(
            model,
            estimated_input_tokens,
            thinking_enabled,
            tool_name_map,
        );
        Self {
            inner,
            event_buffer: Vec::new(),
            initial_events_generated: false,
        }
    }

    /// 注入由 CacheMeter 计算的缓存覆盖情况（estimate 口径），最终上报时分摊。
    pub fn set_cache_usage(&mut self, cache_usage: super::cache_metering::CacheUsage) {
        self.inner.cache_usage = cache_usage;
    }

    /// 处理 Kiro 事件并缓冲结果
    ///
    /// 复用 StreamContext 的事件处理逻辑，但把结果缓存而不是立即发送。
    pub fn process_and_buffer(&mut self, event: &crate::kiro::model::events::Event) {
        // 首次处理事件时，先生成初始事件（message_start 等）
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 处理事件并缓冲结果
        let events = self.inner.process_kiro_event(event);
        self.event_buffer.extend(events);
    }

    /// 完成流处理并返回所有事件
    ///
    /// 此方法会：
    /// 1. 生成最终事件（message_delta, message_stop）
    /// 2. 用正确的 input_tokens 更正 message_start 事件
    /// 3. 返回所有缓冲的事件
    pub fn finish_and_get_all_events(&mut self) -> Vec<SseEvent> {
        // 如果从未处理过事件，也要生成初始事件
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        let (final_input_tokens, cache_creation, cache_read) = self.inner.resolved_usage();

        // 生成最终事件（StreamContext 内部会用同样的优先级）
        let final_events = self.inner.generate_final_events();
        self.event_buffer.extend(final_events);

        // 更正 message_start 事件中的 input_tokens 与 cache_* 字段
        for event in &mut self.event_buffer {
            if event.event == "message_start" {
                if let Some(message) = event.data.get_mut("message") {
                    if let Some(usage) = message.get_mut("usage") {
                        usage["input_tokens"] = serde_json::json!(final_input_tokens);
                        usage["cache_creation_input_tokens"] = serde_json::json!(cache_creation);
                        usage["cache_read_input_tokens"] = serde_json::json!(cache_read);
                    }
                }
            }
        }

        std::mem::take(&mut self.event_buffer)
    }

    pub fn tool_json_error_message(&self) -> Option<String> {
        self.inner.tool_json_error.as_ref().map(|err| err.message())
    }

    /// 取出最终用量（在 finish_and_get_all_events 之后调用）
    ///
    /// 返回顺序：(input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, credits)
    pub fn final_usage(&self) -> (i32, i32, i32, i32, f64) {
        let (final_input, cache_creation, cache_read) = self.inner.resolved_usage();
        (
            final_input,
            self.inner.output_tokens,
            cache_creation,
            cache_read,
            self.inner.credits,
        )
    }
}

/// 简单的 token 估算（中英文字符混合）
///
/// 公开供 cache_meter 等模块复用同一估算口径。
pub fn estimate_tokens(text: &str) -> i32 {
    let chars: Vec<char> = text.chars().collect();
    let mut chinese_count = 0;
    let mut other_count = 0;

    for c in &chars {
        if *c >= '\u{4E00}' && *c <= '\u{9FFF}' {
            chinese_count += 1;
        } else {
            other_count += 1;
        }
    }

    // 中文约 1.5 字符/token，英文约 4 字符/token
    let chinese_tokens = (chinese_count * 2 + 2) / 3;
    let other_tokens = (other_count + 3) / 4;

    (chinese_tokens + other_tokens).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_event_format() {
        let event = SseEvent::new("message_start", json!({"type": "message_start"}));
        let sse_str = event.to_sse_string();

        assert!(sse_str.starts_with("event: message_start\n"));
        assert!(sse_str.contains("data: "));
        assert!(sse_str.ends_with("\n\n"));
    }

    #[test]
    fn test_sse_state_manager_message_start() {
        let mut manager = SseStateManager::new();

        // 第一次应该成功
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_some());

        // 第二次应该被跳过
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_none());
    }

    #[test]
    fn test_message_start_includes_provisional_cache_usage() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 100, false, HashMap::new());
        ctx.cache_usage = crate::anthropic::cache_metering::CacheUsage {
            cache_read: 30,
            cache_covered_est: 80,
            prompt_total_est: 100,
        };

        let event = ctx.create_message_start_event();
        let usage = &event["message"]["usage"];

        assert_eq!(usage["input_tokens"], json!(20));
        assert_eq!(usage["cache_creation_input_tokens"], json!(50));
        assert_eq!(usage["cache_read_input_tokens"], json!(30));
    }

    #[test]
    fn cc_v1_message_start_waits_for_context_usage_event() {
        let mut ctx = BufferedStreamContext::new("test-model", 100, false, HashMap::new());
        ctx.set_cache_usage(crate::anthropic::cache_metering::CacheUsage {
            cache_read: 25,
            cache_covered_est: 70,
            prompt_total_est: 100,
        });

        ctx.process_and_buffer(&Event::AssistantResponse(
            serde_json::from_value(serde_json::json!({"content": "hello"})).unwrap(),
        ));
        ctx.process_and_buffer(&Event::ContextUsage(
            serde_json::from_value(serde_json::json!({"contextUsagePercentage": 50.0})).unwrap(),
        ));
        let final_events = ctx.finish_and_get_all_events();

        let message_start = final_events
            .iter()
            .find(|e| e.event == "message_start")
            .expect("final buffered flush should include message_start");
        let start_usage = &message_start.data["message"]["usage"];
        assert_ne!(
            start_usage["input_tokens"],
            json!(30),
            "message_start must not use provisional fallback usage"
        );
        let message_delta = final_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");
        let usage = &message_delta.data["usage"];
        for key in [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ] {
            assert_eq!(
                start_usage[key], usage[key],
                "{key} must match between message_start and message_delta after context usage is known"
            );
        }
    }

    #[test]
    fn test_sse_state_manager_block_lifecycle() {
        let mut manager = SseStateManager::new();

        // 创建块
        let events = manager.handle_content_block_start(0, "text", json!({}));
        assert_eq!(events.len(), 1);

        // delta
        let event = manager.handle_content_block_delta(0, json!({}));
        assert!(event.is_some());

        // stop
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_some());

        // 重复 stop 应该被跳过
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_none());
    }

    #[test]
    fn test_tool_name_reverse_mapping_in_stream() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert(
            "short_abc12345".to_string(),
            "mcp__very_long_original_tool_name".to_string(),
        );

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, map);
        let _ = ctx.generate_initial_events();

        // 模拟 Kiro 返回短名称的 tool_use
        let tool_event = Event::ToolUse(ToolUseEvent {
            name: "short_abc12345".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"key":"value"}"#.to_string(),
            stop: true,
        });

        let events = ctx.process_kiro_event(&tool_event);

        // content_block_start 中的 name 应该是原始长名称
        let start_event = events
            .iter()
            .find(|e| e.event == "content_block_start")
            .unwrap();
        assert_eq!(
            start_event.data["content_block"]["name"], "mcp__very_long_original_tool_name",
            "应还原为原始工具名称"
        );
    }

    #[test]
    fn test_tool_json_accumulator_waits_until_stop() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert("fs_write".to_string(), "Write".to_string());
        let mut acc = ToolJsonAccumulator::new();

        let first = acc
            .push(
                &ToolUseEvent {
                    name: "fs_write".to_string(),
                    tool_use_id: "tool_1".to_string(),
                    input: r#"{"path":"/tmp/a.txt","#.to_string(),
                    stop: false,
                },
                &map,
            )
            .unwrap();
        assert!(first.is_none());

        let completed = acc
            .push(
                &ToolUseEvent {
                    name: "fs_write".to_string(),
                    tool_use_id: "tool_1".to_string(),
                    input: r#""text":"hello"}"#.to_string(),
                    stop: true,
                },
                &map,
            )
            .unwrap()
            .unwrap();

        assert_eq!(completed.name, "Write");
        assert_eq!(completed.input["file_path"], "/tmp/a.txt");
        assert_eq!(completed.input["content"], "hello");
    }

    #[test]
    fn test_tool_json_accumulator_restores_kiro_equivalent_claude_code_builtin_tools() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        for (kiro_name, claude_name) in [
            ("fs_write", "Write"),
            ("str_replace", "Edit"),
            ("execute_bash", "Bash"),
            ("read_file", "Read"),
            ("file_search", "Glob"),
            ("grep_search", "Grep"),
            ("list_directory", "LS"),
            ("web_search", "WebSearch"),
        ] {
            map.insert(kiro_name.to_string(), claude_name.to_string());
        }

        let cases = vec![
            (
                "fs_write",
                serde_json::json!({"path": "/tmp/a.txt", "text": "hello"}),
                "Write",
                serde_json::json!({"file_path": "/tmp/a.txt", "content": "hello"}),
            ),
            (
                "str_replace",
                serde_json::json!({
                    "path": "/tmp/a.txt",
                    "oldStr": "old",
                    "newStr": "new"
                }),
                "Edit",
                serde_json::json!({
                    "file_path": "/tmp/a.txt",
                    "old_string": "old",
                    "new_string": "new"
                }),
            ),
            (
                "execute_bash",
                serde_json::json!({"command": "echo ok", "timeout": 1000}),
                "Bash",
                serde_json::json!({"command": "echo ok", "timeout": 1000}),
            ),
            (
                "read_file",
                serde_json::json!({"path": "/tmp/a.txt", "start_line": 10, "end_line": 12}),
                "Read",
                serde_json::json!({"file_path": "/tmp/a.txt", "offset": 10, "limit": 3}),
            ),
            (
                "file_search",
                serde_json::json!({"query": "**/*.rs"}),
                "Glob",
                serde_json::json!({"pattern": "**/*.rs"}),
            ),
            (
                "grep_search",
                serde_json::json!({
                    "query": "fn main",
                    "includePattern": "**/*.rs",
                    "caseSensitive": true
                }),
                "Grep",
                serde_json::json!({
                    "pattern": "fn main",
                    "glob": "**/*.rs",
                    "case_sensitive": true
                }),
            ),
            (
                "list_directory",
                serde_json::json!({"path": "/tmp"}),
                "LS",
                serde_json::json!({"path": "/tmp"}),
            ),
            (
                "web_search",
                serde_json::json!({"query": "Kiro CLI 2.6.0"}),
                "WebSearch",
                serde_json::json!({"query": "Kiro CLI 2.6.0"}),
            ),
        ];

        for (index, (kiro_name, kiro_input, expected_name, expected_input)) in
            cases.into_iter().enumerate()
        {
            let mut acc = ToolJsonAccumulator::new();
            let completed = acc
                .push(
                    &ToolUseEvent {
                        name: kiro_name.to_string(),
                        tool_use_id: format!("tool_{index}"),
                        input: kiro_input.to_string(),
                        stop: true,
                    },
                    &map,
                )
                .unwrap()
                .unwrap();

            assert_eq!(completed.name, expected_name, "{kiro_name} name mismatch");
            assert_eq!(
                completed.input, expected_input,
                "{kiro_name} input mismatch"
            );
        }
    }

    #[test]
    fn test_tool_json_accumulator_parallel_tool_uses() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut acc = ToolJsonAccumulator::new();
        let map = HashMap::new();

        assert!(
            acc.push(
                &ToolUseEvent {
                    name: "a".to_string(),
                    tool_use_id: "tool_a".to_string(),
                    input: r#"{"a":"#.to_string(),
                    stop: false,
                },
                &map,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            acc.push(
                &ToolUseEvent {
                    name: "b".to_string(),
                    tool_use_id: "tool_b".to_string(),
                    input: r#"{"b":2}"#.to_string(),
                    stop: true,
                },
                &map,
            )
            .unwrap()
            .is_some()
        );
        let completed_a = acc
            .push(
                &ToolUseEvent {
                    name: "a".to_string(),
                    tool_use_id: "tool_a".to_string(),
                    input: r#""one"}"#.to_string(),
                    stop: true,
                },
                &map,
            )
            .unwrap()
            .unwrap();

        assert_eq!(completed_a.input["a"], "one");
    }

    #[test]
    fn test_tool_json_accumulator_invalid_json() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut acc = ToolJsonAccumulator::new();
        let err = acc
            .push(
                &ToolUseEvent {
                    name: "fs_write".to_string(),
                    tool_use_id: "tool_bad".to_string(),
                    input: r#"{"path":"#.to_string(),
                    stop: true,
                },
                &HashMap::new(),
            )
            .unwrap_err();

        assert_eq!(err.error_type(), "upstream_tool_json_error");
        assert!(err.message().contains("tool_bad"));
    }

    #[test]
    fn test_tool_json_accumulator_reports_incomplete_on_finish() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut acc = ToolJsonAccumulator::new();
        let partial = acc
            .push(
                &ToolUseEvent {
                    name: "fs_write".to_string(),
                    tool_use_id: "tool_truncated".to_string(),
                    input: r#"{"path":"/tmp/a.txt","text":"#.to_string(),
                    stop: false,
                },
                &HashMap::new(),
            )
            .unwrap();
        assert!(partial.is_none());

        let err = acc.finish(&HashMap::new()).unwrap_err();
        assert_eq!(err.error_type(), "upstream_tool_json_error");
        assert!(err.message().contains("tool_truncated"));
        assert!(err.message().contains("ended before completing"));
    }

    // 回归测试（原阶段 1A 复现测试，阶段 3A 翻转为修复后期望）：
    // 空参数工具（EnterPlanMode/ExitPlanMode/Agent）上游只发 toolUse 事件、未补 stop=true，
    // 留下 0 字节 dangling buffer。修复后 finish() 不再报错，而是把它当作合法 input:{} 工具补发。
    // 与 push() 对"收到 stop 的空输入"的处理保持一致；这消除了 sub2api 的 "buffered 0 bytes" 502。
    #[test]
    fn test_enterplanmode_empty_buffer_emitted_as_valid_tool() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut acc = ToolJsonAccumulator::new();
        let pending = acc
            .push(
                &ToolUseEvent {
                    name: "EnterPlanMode".to_string(),
                    tool_use_id: "tp".to_string(),
                    input: String::new(),
                    stop: false,
                },
                &HashMap::new(),
            )
            .unwrap();
        assert!(pending.is_none());

        // 修复后：finish() 返回 Ok，并把空参数工具作为合法 tool_use 补发。
        let completed = acc.finish(&HashMap::new()).unwrap();
        assert_eq!(completed.len(), 1, "空参数工具应被补发为 1 个合法 tool_use");
        assert_eq!(completed[0].id, "tp");
        assert_eq!(completed[0].name, "EnterPlanMode");
        assert_eq!(completed[0].input, serde_json::json!({}));
    }

    // 同时验证：空缓冲被补发后，accumulator 不再残留，再次 finish() 为干净的 Ok(vec![])。
    #[test]
    fn test_finish_empty_and_truncated_mixed() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut acc = ToolJsonAccumulator::new();
        // 一个空参数工具（应补发）
        acc.push(
            &ToolUseEvent {
                name: "EnterPlanMode".to_string(),
                tool_use_id: "empty".to_string(),
                input: String::new(),
                stop: false,
            },
            &HashMap::new(),
        )
        .unwrap();
        // 一个真正被截断的半截 JSON（应仍报错，防护保留）
        acc.push(
            &ToolUseEvent {
                name: "fs_write".to_string(),
                tool_use_id: "truncated".to_string(),
                input: r#"{"path":"/tmp/a.txt","text":"#.to_string(),
                stop: false,
            },
            &HashMap::new(),
        )
        .unwrap();

        // 混合场景：半截 JSON 仍触发 IncompleteJson（不会被空缓冲处理掩盖）。
        let err = acc.finish(&HashMap::new()).unwrap_err();
        assert_eq!(err.error_type(), "upstream_tool_json_error");
        assert!(err.message().contains("truncated"));
        assert!(err.message().contains("ended before completing"));
    }

    #[test]
    fn test_stream_tool_use_emits_complete_json_once() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert("fs_write".to_string(), "Write".to_string());
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, map);
        let _ = ctx.generate_initial_events();

        let part = ctx.process_kiro_event(&Event::ToolUse(ToolUseEvent {
            name: "fs_write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: r#"{"path":"/tmp/a.txt","#.to_string(),
            stop: false,
        }));
        assert!(
            part.iter().all(|e| e.event != "content_block_start"
                || e.data["content_block"]["type"] != "tool_use"),
            "partial input should not start a tool_use block"
        );

        let done = ctx.process_kiro_event(&Event::ToolUse(ToolUseEvent {
            name: "fs_write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: r#""text":"hello"}"#.to_string(),
            stop: true,
        }));
        let start = done
            .iter()
            .find(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
            })
            .unwrap();
        assert_eq!(start.data["content_block"]["name"], "Write");

        let delta = done
            .iter()
            .find(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "input_json_delta"
            })
            .unwrap();
        assert_eq!(
            delta.data["delta"]["partial_json"],
            serde_json::json!({"file_path": "/tmp/a.txt", "content": "hello"}).to_string()
        );
    }

    #[test]
    fn test_stream_tool_use_incomplete_on_finish_emits_error_without_tool_block() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert("fs_write".to_string(), "Write".to_string());
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, map);
        let _ = ctx.generate_initial_events();

        let part = ctx.process_kiro_event(&Event::ToolUse(ToolUseEvent {
            name: "fs_write".to_string(),
            tool_use_id: "tool_truncated".to_string(),
            input: r#"{"path":"/tmp/a.txt","text":"#.to_string(),
            stop: false,
        }));
        assert!(
            part.iter().all(|e| e.event != "content_block_start"
                || e.data["content_block"]["type"] != "tool_use"),
            "partial input should not start a tool_use block"
        );

        let final_events = ctx.generate_final_events();
        assert!(
            final_events.iter().all(|e| {
                e.event != "content_block_start" || e.data["content_block"]["type"] != "tool_use"
            }),
            "incomplete input must not be forwarded as a tool_use block"
        );
        let error = final_events.iter().find(|e| e.event == "error").unwrap();
        assert_eq!(
            error.data["error"]["type"],
            serde_json::json!("upstream_tool_json_error")
        );
        assert!(
            error.data["error"]["message"]
                .as_str()
                .unwrap()
                .contains("tool_truncated")
        );
    }

    // 流式：空参数工具（如 EnterPlanMode）在流结束时仍未收到 stop，
    // generate_final_events 应把它补发为合法 tool_use 块（input {}），而非报错。
    #[test]
    fn test_stream_empty_param_tool_emitted_at_finish() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        // 上游只发了 toolUse 事件（空 input、无 stop），未补 stop 分片。
        let part = ctx.process_kiro_event(&Event::ToolUse(ToolUseEvent {
            name: "EnterPlanMode".to_string(),
            tool_use_id: "tp".to_string(),
            input: String::new(),
            stop: false,
        }));
        assert!(
            part.iter().all(|e| e.event != "content_block_start"
                || e.data["content_block"]["type"] != "tool_use"),
            "未 stop 前不应开 tool_use 块"
        );

        let final_events = ctx.generate_final_events();
        // 不应有 error 块
        assert!(
            final_events.iter().all(|e| e.event != "error"),
            "空参数工具不应触发 error 块"
        );
        // 应补发一个 EnterPlanMode 的 tool_use 块（input 为空对象）
        let start = final_events
            .iter()
            .find(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
            })
            .expect("空参数工具应被补发为 tool_use 块");
        assert_eq!(start.data["content_block"]["name"], "EnterPlanMode");
        assert_eq!(start.data["content_block"]["id"], "tp");
        // 块序应有对应的 content_block_stop
        assert!(
            final_events
                .iter()
                .any(|e| e.event == "content_block_stop"),
            "补发的 tool_use 块应正常闭合"
        );
    }

    // 流式混合：一个空参数工具 + 一个被截断的半截 JSON 同时 dangling。
    // 半截 JSON 仍应触发 error 块，且**绝不能**为半截工具发出任何 tool_use 块。
    // （空工具是否补发不在本断言范围——本测试聚焦防护未被掩盖。）
    #[test]
    fn test_stream_mixed_empty_and_truncated_emits_error_no_partial_block() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert("fs_write".to_string(), "Write".to_string());
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, map);
        let _ = ctx.generate_initial_events();

        ctx.process_kiro_event(&Event::ToolUse(ToolUseEvent {
            name: "EnterPlanMode".to_string(),
            tool_use_id: "empty".to_string(),
            input: String::new(),
            stop: false,
        }));
        ctx.process_kiro_event(&Event::ToolUse(ToolUseEvent {
            name: "fs_write".to_string(),
            tool_use_id: "truncated".to_string(),
            input: r#"{"path":"/tmp/a.txt","text":"#.to_string(),
            stop: false,
        }));

        let final_events = ctx.generate_final_events();

        // 半截 JSON 触发 error 块
        let error = final_events
            .iter()
            .find(|e| e.event == "error")
            .expect("半截 JSON 应触发 error 块");
        assert_eq!(
            error.data["error"]["type"],
            serde_json::json!("upstream_tool_json_error")
        );
        assert!(
            error.data["error"]["message"]
                .as_str()
                .unwrap()
                .contains("truncated")
        );
        // 绝不能为被截断的工具发出 tool_use 块（防止半截参数泄漏给客户端）
        assert!(
            final_events.iter().all(|e| {
                e.event != "content_block_start"
                    || e.data["content_block"]["type"] != "tool_use"
                    || e.data["content_block"]["id"] != "truncated"
            }),
            "被截断的工具绝不能被发出为 tool_use 块"
        );
    }

    #[test]
    fn test_text_delta_after_tool_use_restarts_text_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());

        let initial_events = ctx.generate_initial_events();
        assert!(
            initial_events
                .iter()
                .any(|e| e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "text")
        );

        let initial_text_index = ctx
            .text_block_index
            .expect("initial text block index should exist");

        // tool_use 开始会自动关闭现有 text block
        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: true,
        });
        assert!(
            tool_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(initial_text_index as i64)
            }),
            "tool_use should stop the previous text block"
        );

        // 之后再来文本增量，应自动创建新的 text block 而不是往已 stop 的块里写 delta
        let text_events = ctx.process_assistant_response("hello");
        let new_text_start_index = text_events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        assert!(
            new_text_start_index.is_some(),
            "should start a new text block"
        );
        assert_ne!(
            new_text_start_index.unwrap(),
            initial_text_index as i64,
            "new text block index should differ from the stopped one"
        );
        assert!(
            text_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "hello"
            }),
            "should emit text_delta after restarting text block"
        );
    }

    #[test]
    fn test_stream_filters_cross_chunk_tool_use_xml_leak() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_assistant_response("before\n<tool"));
        all_events.extend(ctx.process_assistant_response(
            "_use id=\"toolu_1\" name=\"Write\">\n{\"path\":\"/tmp/a\"}\n</tool_use>\nafter",
        ));
        all_events.extend(ctx.generate_final_events());

        let text = all_events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect::<String>();

        assert!(text.contains("before"));
        assert!(text.contains("after"));
        assert!(!text.contains("<tool_use"));
        assert!(!text.contains("\"path\""));
    }

    #[test]
    fn test_stream_filters_cross_chunk_truncated_tool_use_xml_leak() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_assistant_response("before\n<tool"));
        all_events.extend(ctx.process_assistant_response("_use id=\"toolu_1\" name=\"Write\""));
        all_events.extend(ctx.generate_final_events());

        let text = all_events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect::<String>();

        assert!(text.contains("before"));
        assert!(!text.contains("<tool_use"));
        assert!(!text.contains("toolu_1"));
    }

    #[test]
    fn test_tool_use_flushes_pending_thinking_buffer_text_before_tool_block() {
        // thinking 模式下，短文本可能被暂存在 thinking_buffer 以等待 `<thinking>` 的跨 chunk 匹配。
        // 当紧接着出现 tool_use 时，应先 flush 这段文本，再开始 tool_use block。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        // 两段短文本（各 2 个中文字符），总长度仍可能不足以满足 safe_len>0 的输出条件，
        // 因而会留在 thinking_buffer 中等待后续 chunk。
        let ev1 = ctx.process_assistant_response("有修");
        assert!(
            ev1.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should be buffered under thinking mode"
        );
        let ev2 = ctx.process_assistant_response("改：");
        assert!(
            ev2.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should still be buffered under thinking mode"
        );

        let events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: true,
        });

        let text_start_index = events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        let pos_text_delta = events.iter().position(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
        });
        let pos_text_stop = text_start_index.and_then(|idx| {
            events.iter().position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(idx)
            })
        });
        let pos_tool_start = events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });

        assert!(
            text_start_index.is_some(),
            "should start a text block to flush buffered text"
        );
        assert!(
            pos_text_delta.is_some(),
            "should flush buffered text as text_delta"
        );
        assert!(
            pos_text_stop.is_some(),
            "should stop text block before tool_use block starts"
        );
        assert!(pos_tool_start.is_some(), "should start tool_use block");

        let pos_text_delta = pos_text_delta.unwrap();
        let pos_text_stop = pos_text_stop.unwrap();
        let pos_tool_start = pos_tool_start.unwrap();

        assert!(
            pos_text_delta < pos_text_stop && pos_text_stop < pos_tool_start,
            "ordering should be: text_delta -> text_stop -> tool_use_start"
        );

        assert!(
            events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "有修改："
            }),
            "flushed text should equal the buffered prefix"
        );
    }

    #[test]
    fn test_estimate_tokens() {
        assert!(estimate_tokens("Hello") > 0);
        assert!(estimate_tokens("你好") > 0);
        assert!(estimate_tokens("Hello 你好") > 0);
    }

    #[test]
    fn test_find_real_thinking_start_tag_basic() {
        // 基本情况：正常的开始标签
        assert_eq!(find_real_thinking_start_tag("<thinking>"), Some(0));
        assert_eq!(find_real_thinking_start_tag("prefix<thinking>"), Some(6));
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("`<thinking>`"), None);
        assert_eq!(find_real_thinking_start_tag("use `<thinking>` tag"), None);

        // 先有被包裹的，后有真正的开始标签
        assert_eq!(
            find_real_thinking_start_tag("about `<thinking>` tag<thinking>content"),
            Some(22)
        );
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("\"<thinking>\""), None);
        assert_eq!(find_real_thinking_start_tag("the \"<thinking>\" tag"), None);

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("'<thinking>'"), None);

        // 混合情况
        assert_eq!(
            find_real_thinking_start_tag("about \"<thinking>\" and '<thinking>' then<thinking>"),
            Some(40)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_basic() {
        // 基本情况：正常的结束标签后面有双换行符
        assert_eq!(find_real_thinking_end_tag("</thinking>\n\n"), Some(0));
        assert_eq!(
            find_real_thinking_end_tag("content</thinking>\n\n"),
            Some(7)
        );
        assert_eq!(
            find_real_thinking_end_tag("some text</thinking>\n\nmore text"),
            Some(9)
        );

        // 兼容 EOF、单换行和直接接正文
        assert_eq!(find_real_thinking_end_tag("</thinking>"), Some(0));
        assert_eq!(find_real_thinking_end_tag("</thinking>\n"), Some(0));
        assert_eq!(find_real_thinking_end_tag("</thinking>\r\n"), Some(0));
        assert_eq!(find_real_thinking_end_tag("</thinking> more"), Some(0));
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("`</thinking>`\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("mention `</thinking>` in code\n\n"),
            None
        );

        // 只有前面有反引号
        assert_eq!(find_real_thinking_end_tag("`</thinking>\n\n"), None);

        // 只有后面有反引号
        assert_eq!(find_real_thinking_end_tag("</thinking>`\n\n"), None);
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("\"</thinking>\"\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("the string \"</thinking>\" is a tag\n\n"),
            None
        );

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("'</thinking>'\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("use '</thinking>' as marker\n\n"),
            None
        );

        // 混合情况：双引号包裹后有真正的标签
        assert_eq!(
            find_real_thinking_end_tag("about \"</thinking>\" tag</thinking>\n\n"),
            Some(23)
        );

        // 混合情况：单引号包裹后有真正的标签
        assert_eq!(
            find_real_thinking_end_tag("about '</thinking>' tag</thinking>\n\n"),
            Some(23)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_mixed() {
        // 先有被包裹的，后有真正的结束标签
        assert_eq!(
            find_real_thinking_end_tag("discussing `</thinking>` tag</thinking>\n\n"),
            Some(28)
        );

        // 多个被包裹的，最后一个是真正的
        assert_eq!(
            find_real_thinking_end_tag("`</thinking>` and `</thinking>` done</thinking>\n\n"),
            Some(36)
        );

        // 多种引用字符混合
        assert_eq!(
            find_real_thinking_end_tag(
                "`</thinking>` and \"</thinking>\" and '</thinking>' done</thinking>\n\n"
            ),
            Some(54)
        );
    }

    #[test]
    fn test_tool_use_immediately_after_thinking_filters_end_tag_and_closes_thinking_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();

        // thinking 内容以 `</thinking>` 结尾，但后面没有 `\n\n`（模拟紧跟 tool_use 的场景）
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));

        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: true,
        });
        all_events.extend(tool_events);

        all_events.extend(ctx.generate_final_events());

        // 不应把 `</thinking>` 当作 thinking 内容输出
        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered from output"
        );

        // thinking block 必须在 tool_use block 之前关闭
        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");
        let pos_thinking_stop = all_events.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        let pos_tool_start = all_events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });
        assert!(
            pos_thinking_stop.is_some(),
            "thinking block should be stopped"
        );
        assert!(pos_tool_start.is_some(), "tool_use block should be started");
        assert!(
            pos_thinking_stop.unwrap() < pos_tool_start.unwrap(),
            "thinking block should stop before tool_use block starts"
        );
    }

    #[test]
    fn test_thinking_block_emits_signature_delta_before_stop() {
        // 客户端在 thinking 模式下要求 thinking 块带 signature 字段，否则下一轮回传时
        // 会抛出 "must be passed back to the API"。本测试验证 thinking 块结束前发送了
        // 一个非空的 signature_delta 事件。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>abc</thinking>\n\nhello"));
        all.extend(ctx.generate_final_events());

        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");

        let pos_sig = all.iter().position(|e| {
            e.event == "content_block_delta"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
        });
        let pos_stop = all.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });

        assert!(pos_sig.is_some(), "signature_delta should be emitted");
        assert!(pos_stop.is_some(), "content_block_stop should be emitted");
        assert!(
            pos_sig.unwrap() < pos_stop.unwrap(),
            "signature_delta must precede content_block_stop"
        );
    }

    #[test]
    fn test_reasoning_content_event_emits_thinking_block_with_placeholder_signature() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let mut all = ctx.generate_initial_events();

        all.extend(
            ctx.process_kiro_event(&Event::ReasoningContent(
                serde_json::from_value(serde_json::json!({
                    "text": "plan",
                    "signature": "sig-upstream"
                }))
                .unwrap(),
            )),
        );
        all.extend(ctx.process_kiro_event(&Event::AssistantResponse(
            serde_json::from_value(serde_json::json!({"content": "answer"})).unwrap(),
        )));
        all.extend(ctx.generate_final_events());

        let thinking_index = ctx
            .thinking_block_index
            .expect("reasoning should create a thinking block");
        let signature = all.iter().find(|event| {
            event.event == "content_block_delta"
                && event.data["index"].as_i64() == Some(thinking_index as i64)
                && event.data["delta"]["type"] == "signature_delta"
        });

        assert_eq!(collect_thinking_content(&all), "plan");
        assert_eq!(collect_text_content(&all), "answer");
        assert_eq!(
            signature.unwrap().data["delta"]["signature"].as_str(),
            Some(THINKING_SIGNATURE_PLACEHOLDER)
        );
    }

    #[test]
    fn test_reasoning_content_event_emits_redacted_thinking_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let mut all = ctx.generate_initial_events();

        all.extend(
            ctx.process_kiro_event(&Event::ReasoningContent(
                serde_json::from_value(serde_json::json!({
                    "redactedContent": "encrypted-thinking"
                }))
                .unwrap(),
            )),
        );
        all.extend(ctx.generate_final_events());

        assert_eq!(
            collect_thinking_content(&all),
            "",
            "redactedContent must not be emitted as plaintext thinking_delta"
        );
        let start = all
            .iter()
            .find(|e| {
                e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "redacted_thinking"
            })
            .expect("redacted thinking block should start");
        assert_eq!(
            start.data["content_block"]["data"].as_str(),
            Some("encrypted-thinking")
        );
    }

    #[test]
    fn thinking_disabled_drops_reasoning_text() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let mut all = ctx.generate_initial_events();

        all.extend(
            ctx.process_kiro_event(&Event::ReasoningContent(
                serde_json::from_value(serde_json::json!({
                    "text": "visible reasoning",
                    "signature": "sig-secret"
                }))
                .unwrap(),
            )),
        );
        all.extend(ctx.generate_final_events());

        assert_eq!(collect_text_content(&all), "");
        assert!(
            all.iter()
                .all(|e| e.data["delta"]["type"] != "signature_delta"),
            "disabled thinking must not leak reasoning signatures"
        );
    }

    #[test]
    fn test_reasoning_only_sets_max_tokens_stop_reason() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let mut all = ctx.generate_initial_events();

        all.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            serde_json::from_value(serde_json::json!({"text": "plan"})).unwrap(),
        )));
        all.extend(ctx.generate_final_events());

        let message_delta = all
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");
        let signature = all.iter().find(|event| {
            event.event == "content_block_delta" && event.data["delta"]["type"] == "signature_delta"
        });

        assert_eq!(collect_thinking_content(&all), "plan");
        assert_eq!(
            signature.unwrap().data["delta"]["signature"].as_str(),
            Some(THINKING_SIGNATURE_PLACEHOLDER)
        );
        assert_eq!(message_delta.data["delta"]["stop_reason"], "max_tokens");
    }

    #[test]
    fn test_final_flush_filters_standalone_thinking_end_tag() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered during final flush"
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_same_chunk() {
        // <thinking>\n 在同一个 chunk 中，\n 应被剥离
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nHello world");

        // 找到所有 thinking_delta 事件
        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        // 拼接所有 thinking 内容
        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_cross_chunk() {
        // <thinking> 在第一个 chunk 末尾，\n 在第二个 chunk 开头
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events1 = ctx.process_assistant_response("<thinking>");
        let events2 = ctx.process_assistant_response("\nHello world");

        let mut all_events = Vec::new();
        all_events.extend(events1);
        all_events.extend(events2);

        let thinking_deltas: Vec<_> = all_events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n across chunks, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_no_strip_when_no_leading_newline() {
        // <thinking> 后直接跟内容（无 \n），内容应完整保留
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>abc</thinking>\n\ntext");

        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .filter(|e| {
                !e.data["delta"]["thinking"]
                    .as_str()
                    .unwrap_or("")
                    .is_empty()
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert_eq!(full_thinking, "abc", "thinking content should be 'abc'");
    }

    #[test]
    fn test_text_after_thinking_strips_leading_newlines() {
        // `</thinking>\n\n` 后的文本不应以 \n\n 开头
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nabc</thinking>\n\n你好");

        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .collect();

        let full_text: String = text_deltas
            .iter()
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_text.starts_with('\n'),
            "text after thinking should not start with \\n, got: {:?}",
            full_text
        );
        assert_eq!(full_text, "你好");
    }

    #[test]
    fn thinking_end_tag_accepts_eof_lf_crlf_whitespace_and_inline_text() {
        for (input, expected_text) in [
            ("<thinking>abc</thinking>", ""),
            ("<thinking>abc</thinking>\n正文", "正文"),
            ("<thinking>abc</thinking>\r\n正文", "正文"),
            ("<thinking>abc</thinking>正文", "正文"),
            ("<thinking>abc</thinking>   \n正文", "正文"),
        ] {
            let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
            let _ = ctx.generate_initial_events();
            let mut all = ctx.process_assistant_response(input);
            all.extend(ctx.generate_final_events());
            assert_eq!(collect_thinking_content(&all), "abc", "input={input:?}");
            assert_eq!(
                collect_text_content(&all).trim(),
                expected_text,
                "input={input:?}"
            );
        }
    }

    /// 辅助函数：从事件列表中提取所有 thinking_delta 的拼接内容
    fn collect_thinking_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 辅助函数：从事件列表中提取所有 text_delta 的拼接内容
    fn collect_text_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect()
    }

    #[test]
    fn test_end_tag_newlines_split_across_events() {
        // `</thinking>\n` 在 chunk 1，`\n` 在 chunk 2，`text` 在 chunk 3
        // 确保 `</thinking>` 不会被部分当作 thinking 内容发出
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_end_tag_alone_in_chunk_then_newlines_in_next() {
        // `</thinking>` 单独在一个 chunk，`\n\ntext` 在下一个 chunk
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all.extend(ctx.process_assistant_response("\n\n你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_start_tag_newline_split_across_events() {
        // `\n\n` 在 chunk 1，`<thinking>` 在 chunk 2，`\n` 在 chunk 3
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("\n\n"));
        all.extend(ctx.process_assistant_response("<thinking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("abc</thinking>\n\ntext"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "text", "text should be 'text', got: {:?}", text);
    }

    #[test]
    fn test_full_flow_maximally_split() {
        // 极端拆分：每个关键边界都在不同 chunk
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        // \n\n<thinking>\n 拆成多段
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("<thin"));
        all.extend(ctx.process_assistant_response("king>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("hello"));
        // </thinking>\n\n 拆成多段
        all.extend(ctx.process_assistant_response("</thi"));
        all.extend(ctx.process_assistant_response("nking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("world"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "hello",
            "thinking should be 'hello', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "world", "text should be 'world', got: {:?}", text);
    }

    #[test]
    fn test_thinking_only_sets_max_tokens_stop_reason() {
        // 整个流只有 thinking 块，没有 text 也没有 tool_use，stop_reason 应为 max_tokens
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "max_tokens",
            "stop_reason should be max_tokens when only thinking is produced"
        );

        // 应补发一套完整的 text 事件（content_block_start + delta 空格 + content_block_stop）
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "text"
            }),
            "should emit text content_block_start"
        );
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == " "
            }),
            "should emit text_delta with a single space"
        );
        // text block 应被 generate_final_events 自动关闭
        let text_block_index = all_events
            .iter()
            .find_map(|e| {
                if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                    e.data["index"].as_i64()
                } else {
                    None
                }
            })
            .expect("text block should exist");
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(text_block_index)
            }),
            "text block should be stopped"
        );
    }

    #[test]
    fn test_thinking_with_text_keeps_end_turn_stop_reason() {
        // thinking + text 的情况，stop_reason 应为 end_turn
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n\nHello"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "end_turn",
            "stop_reason should be end_turn when text is also produced"
        );
    }

    #[test]
    fn test_thinking_with_tool_use_keeps_tool_use_stop_reason() {
        // thinking + tool_use 的情况，stop_reason 应为 tool_use
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "test_tool".to_string(),
                tool_use_id: "tool_1".to_string(),
                input: "{}".to_string(),
                stop: true,
            }),
        );
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "tool_use",
            "stop_reason should be tool_use when tool_use is present"
        );
    }
}
