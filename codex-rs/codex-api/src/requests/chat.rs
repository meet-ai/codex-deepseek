use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::headers::build_conversation_headers;
use crate::requests::headers::insert_header;
use crate::requests::headers::subagent_header;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use http::HeaderMap;
use http::StatusCode;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;

/// Assembled request body plus headers for Chat Completions streaming calls.
pub struct ChatRequest {
    pub body: Value,
    pub headers: HeaderMap,
}

pub struct ChatRequestBuilder<'a> {
    model: &'a str,
    instructions: &'a str,
    input: &'a [ResponseItem],
    tools: &'a [Value],
    reasoning_content: Option<String>,
    conversation_id: Option<String>,
    session_source: Option<SessionSource>,
}

impl<'a> ChatRequestBuilder<'a> {
    pub fn new(
        model: &'a str,
        instructions: &'a str,
        input: &'a [ResponseItem],
        tools: &'a [Value],
        reasoning_content: Option<String>,
    ) -> Self {
        Self {
            model,
            instructions,
            input,
            tools,
            reasoning_content,
            conversation_id: None,
            session_source: None,
        }
    }

    pub fn conversation_id(mut self, id: Option<String>) -> Self {
        self.conversation_id = id;
        self
    }

    pub fn session_source(mut self, source: Option<SessionSource>) -> Self {
        self.session_source = source;
        self
    }

    pub fn build(self, _provider: &Provider) -> Result<ChatRequest, ApiError> {
        let mut messages = Vec::<Value>::new();
        messages.push(json!({"role": "system", "content": self.instructions}));

        let input = self.input;
        let mut reasoning_by_anchor_index: HashMap<usize, String> = HashMap::new();
        let mut last_emitted_role: Option<&str> = None;
        for item in input {
            match item {
                ResponseItem::Message { role, .. } => last_emitted_role = Some(role.as_str()),
                ResponseItem::FunctionCall { .. } | ResponseItem::LocalShellCall { .. } => {
                    last_emitted_role = Some("assistant")
                }
                ResponseItem::FunctionCallOutput { .. } => last_emitted_role = Some("tool"),
                ResponseItem::Reasoning { .. } | ResponseItem::Other => {}
                ResponseItem::CustomToolCall { .. } => {}
                ResponseItem::CustomToolCallOutput { .. } => {}
                ResponseItem::WebSearchCall { .. } => {}
                ResponseItem::GhostSnapshot { .. } => {}
                ResponseItem::Compaction { .. } => {}
            }
        }

        let mut last_user_index: Option<usize> = None;
        for (idx, item) in input.iter().enumerate() {
            if let ResponseItem::Message { role, .. } = item
                && role == "user"
            {
                last_user_index = Some(idx);
            }
        }

        if !matches!(last_emitted_role, Some("user")) {
            for (idx, item) in input.iter().enumerate() {
                if let Some(u_idx) = last_user_index
                    && idx <= u_idx
                {
                    continue;
                }

                if let ResponseItem::Reasoning {
                    content: Some(items),
                    ..
                } = item
                {
                    let mut text = String::new();
                    for entry in items {
                        match entry {
                            ReasoningItemContent::ReasoningText { text: segment }
                            | ReasoningItemContent::Text { text: segment } => {
                                text.push_str(segment)
                            }
                        }
                    }
                    if text.trim().is_empty() {
                        continue;
                    }

                    let mut attached = false;
                    if idx > 0
                        && let ResponseItem::Message { role, .. } = &input[idx - 1]
                        && role == "assistant"
                    {
                        reasoning_by_anchor_index
                            .entry(idx - 1)
                            .and_modify(|v| v.push_str(&text))
                            .or_insert(text.clone());
                        attached = true;
                    }

                    if !attached && idx + 1 < input.len() {
                        match &input[idx + 1] {
                            ResponseItem::FunctionCall { .. }
                            | ResponseItem::LocalShellCall { .. } => {
                                reasoning_by_anchor_index
                                    .entry(idx + 1)
                                    .and_modify(|v| v.push_str(&text))
                                    .or_insert(text.clone());
                            }
                            ResponseItem::Message { role, .. } if role == "assistant" => {
                                reasoning_by_anchor_index
                                    .entry(idx + 1)
                                    .and_modify(|v| v.push_str(&text))
                                    .or_insert(text.clone());
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        let mut last_assistant_text: Option<String> = None;
        // Track pending tool calls that haven't been responded to yet
        let mut pending_tool_call_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut last_assistant_with_tool_calls_index: Option<usize> = None;

        tracing::warn!("🔄 开始处理{}个输入项", input.len());
        for (idx, item) in input.iter().enumerate() {
            tracing::warn!(
                "📝 处理输入项 {}/{} - 类型: {}",
                idx + 1,
                input.len(),
                match item {
                    ResponseItem::Message { .. } => "Message",
                    ResponseItem::FunctionCall { .. } => "FunctionCall",
                    ResponseItem::FunctionCallOutput { .. } => "FunctionCallOutput",
                    _ => "Other",
                }
            );
            match item {
                ResponseItem::Message {
                    role,
                    content,
                    id,
                    reasoning_content: _,
                    tool_calls,
                } => {
                    // Convert "developer" role to "user" role as Chat Completions API doesn't support "developer" role.
                    let role = if role == "developer" {
                        "user"
                    } else {
                        role.as_str()
                    };

                    // Process content first
                    let mut text = String::new();
                    let mut items: Vec<Value> = Vec::new();
                    let mut saw_image = false;

                    for c in content {
                        match c {
                            ContentItem::InputText { text: t }
                            | ContentItem::OutputText { text: t } => {
                                text.push_str(t);
                                items.push(json!({"type":"text","text": t}));
                            }
                            ContentItem::InputImage { image_url } => {
                                saw_image = true;
                                items.push(
                                    json!({"type":"image_url","image_url": {"url": image_url}}),
                                );
                            }
                        }
                    }

                    // Special handling for tool messages - extract tool_call_id from id field
                    if role == "tool" {
                        if let Some(call_id) = id {
                            // For DeepSeek, tool content should be a simple string, not complex objects
                            let content_value = json!(text);

                            tracing::warn!(
                                "🔧 构建tool消息 - call_id: {}, content_length: {}",
                                call_id,
                                text.len()
                            );
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": call_id,
                                "content": content_value,
                            }));
                            continue;
                        } else {
                            tracing::warn!("⚠️ tool消息缺少call_id，跳过处理");
                        }
                    }

                    // Special handling for assistant messages in DeepSeek thinking mode
                    if role == "assistant" {
                        // For DeepSeek reasoner models, assistant messages need reasoning_content
                        let reasoning_content = match &self.reasoning_content {
                            Some(content) => json!(content),
                            None => json!(""), // DeepSeek文档要求必须有reasoning_content
                        };
                        tracing::warn!(
                            "🤖 DeepSeek assistant消息 - reasoning_content长度: {}",
                            reasoning_content.as_str().unwrap_or("").len()
                        );

                        let mut message = json!({
                            "role": "assistant",
                            "content": text,
                            "reasoning_content": reasoning_content,
                        });

                        // 如果assistant消息包含tool_calls，添加到消息中
                        if let Some(tool_calls) = tool_calls {
                            if !tool_calls.is_empty() {
                                message["tool_calls"] = json!(tool_calls);
                                tracing::warn!(
                                    "🤖 DeepSeek assistant消息包含tool_calls: {}",
                                    tool_calls.len()
                                );
                            }
                        }

                        messages.push(message);
                        continue;
                    }

                    // If we encounter a user or assistant message while there are pending tool calls,
                    // remove the last assistant message with tool_calls (it's incomplete)
                    if (role == "user" || role == "assistant")
                        && !pending_tool_call_ids.is_empty()
                        && last_assistant_with_tool_calls_index.is_some()
                    {
                        // Remove the incomplete assistant message with tool_calls
                        messages.retain(|msg| {
                            !(msg.get("role").and_then(Value::as_str) == Some("assistant")
                                && msg.get("tool_calls").is_some())
                        });
                        pending_tool_call_ids.clear();
                        last_assistant_with_tool_calls_index = None;
                    }

                    let mut text = String::new();
                    let mut items: Vec<Value> = Vec::new();
                    let mut saw_image = false;

                    for c in content {
                        match c {
                            ContentItem::InputText { text: t }
                            | ContentItem::OutputText { text: t } => {
                                text.push_str(t);
                                items.push(json!({"type":"text","text": t}));
                            }
                            ContentItem::InputImage { image_url } => {
                                saw_image = true;
                                items.push(
                                    json!({"type":"image_url","image_url": {"url": image_url}}),
                                );
                            }
                        }
                    }

                    if role == "assistant" {
                        if let Some(prev) = &last_assistant_text
                            && prev == &text
                        {
                            continue;
                        }
                        last_assistant_text = Some(text.clone());
                    }

                    let content_value = if role == "assistant" {
                        json!(text)
                    } else if saw_image {
                        json!(items)
                    } else {
                        json!(text)
                    };

                    let mut msg = json!({"role": role, "content": content_value});
                    if role == "assistant"
                        && let Some(reasoning) = reasoning_by_anchor_index.get(&idx)
                        && let Some(obj) = msg.as_object_mut()
                    {
                        obj.insert("reasoning".to_string(), json!(reasoning));
                    }
                    messages.push(msg);
                }
                ResponseItem::FunctionCall {
                    name,
                    arguments,
                    call_id,
                    ..
                } => {
                    tracing::warn!("🔧 处理FunctionCall - 工具: {}, call_id: {}", name, call_id);
                    let reasoning = reasoning_by_anchor_index.get(&idx).map(String::as_str);
                    let tool_call = json!({
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments,
                        }
                    });
                    pending_tool_call_ids.insert(call_id.clone());
                    push_tool_call_message(&mut messages, tool_call, reasoning);
                    // Track that we just added an assistant message with tool_calls
                    if let Some(last_msg) = messages.last()
                        && last_msg.get("role").and_then(Value::as_str) == Some("assistant")
                        && last_msg.get("tool_calls").is_some()
                    {
                        last_assistant_with_tool_calls_index = Some(idx);
                    }
                }
                ResponseItem::LocalShellCall {
                    id,
                    call_id: _,
                    status,
                    action,
                } => {
                    let reasoning = reasoning_by_anchor_index.get(&idx).map(String::as_str);
                    let call_id = id.clone().unwrap_or_default();
                    let call_id_clone = call_id.clone();
                    let tool_call = json!({
                        "id": call_id_clone.clone(),
                        "type": "local_shell_call",
                        "status": status,
                        "action": action,
                    });
                    pending_tool_call_ids.insert(call_id_clone);
                    push_tool_call_message(&mut messages, tool_call, reasoning);
                    // Track that we just added an assistant message with tool_calls
                    if let Some(last_msg) = messages.last()
                        && last_msg.get("role").and_then(Value::as_str) == Some("assistant")
                        && last_msg.get("tool_calls").is_some()
                    {
                        last_assistant_with_tool_calls_index = Some(idx);
                    }
                }
                ResponseItem::FunctionCallOutput { call_id, output } => {
                    tracing::warn!(
                        "📤 处理FunctionCallOutput - call_id: {}, 输出长度: {}",
                        call_id,
                        output.content.len()
                    );
                    // Remove this call_id from pending set
                    pending_tool_call_ids.remove(call_id.as_str());
                    if pending_tool_call_ids.is_empty() {
                        last_assistant_with_tool_calls_index = None;
                    }

                    let content_value = if let Some(items) = &output.content_items {
                        let mapped: Vec<Value> = items
                            .iter()
                            .map(|it| match it {
                                FunctionCallOutputContentItem::InputText { text } => {
                                    json!({"type":"text","text": text})
                                }
                                FunctionCallOutputContentItem::InputImage { image_url } => {
                                    json!({"type":"image_url","image_url": {"url": image_url}})
                                }
                            })
                            .collect();
                        json!(mapped)
                    } else {
                        json!(output.content)
                    };

                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": content_value,
                    }));
                }
                ResponseItem::CustomToolCall {
                    id,
                    call_id,
                    name,
                    input,
                    status: _,
                } => {
                    // Use call_id for tracking, but id (if present) for the tool_call JSON
                    let tool_call_id = id.clone().unwrap_or_else(|| call_id.clone());
                    let call_id_for_tracking = call_id.clone();
                    let tool_call = json!({
                        "id": tool_call_id,
                        "type": "custom",
                        "custom": {
                            "name": name,
                            "input": input,
                        }
                    });
                    pending_tool_call_ids.insert(call_id_for_tracking);
                    let reasoning = reasoning_by_anchor_index.get(&idx).map(String::as_str);
                    push_tool_call_message(&mut messages, tool_call, reasoning);
                    // Track that we just added an assistant message with tool_calls
                    if let Some(last_msg) = messages.last()
                        && last_msg.get("role").and_then(Value::as_str) == Some("assistant")
                        && last_msg.get("tool_calls").is_some()
                    {
                        last_assistant_with_tool_calls_index = Some(idx);
                    }
                }
                ResponseItem::CustomToolCallOutput { call_id, output } => {
                    // Remove this call_id from pending set
                    pending_tool_call_ids.remove(call_id.as_str());
                    if pending_tool_call_ids.is_empty() {
                        last_assistant_with_tool_calls_index = None;
                    }

                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": output,
                    }));
                }
                ResponseItem::GhostSnapshot { .. } => {
                    continue;
                }
                ResponseItem::Reasoning { .. }
                | ResponseItem::WebSearchCall { .. }
                | ResponseItem::Other
                | ResponseItem::Compaction { .. } => {
                    continue;
                }
            }
        }

        // Validate that every assistant message with tool_calls (except possibly the last one) is followed by corresponding tool messages
        // The last message may have tool_calls without tool responses if it's the start of the current request
        //
        // TODO: For DeepSeek compatibility, we currently skip this validation when we have tool messages
        // because our conversion creates assistant + tool message pairs that don't have proper tool_calls in assistant
        let has_tool_messages = messages
            .iter()
            .any(|msg| msg.get("role").and_then(Value::as_str) == Some("tool"));
        if !has_tool_messages {
            validate_tool_calls_sequence(&messages)?;
        } else {
            tracing::warn!("⚠️ 检测到tool消息，跳过tool_calls序列验证 (DeepSeek兼容模式)");
        }

        let payload = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "tools": self.tools,
        });

        tracing::warn!("✅ 消息处理完成 - 生成了{}条API消息", messages.len());

        // 统计不同类型的消息
        let mut message_counts = std::collections::HashMap::new();
        for msg in &messages {
            if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
                *message_counts.entry(role).or_insert(0) += 1;
            }
        }
        tracing::warn!("📊 消息类型统计: {:?}", message_counts);

        tracing::warn!(
            "📦 API请求payload构建完成 - 消息数: {}, 模型: {}",
            messages.len(),
            self.model
        );

        let mut headers = build_conversation_headers(self.conversation_id);
        if let Some(subagent) = subagent_header(&self.session_source) {
            insert_header(&mut headers, "x-openai-subagent", &subagent);
        }

        tracing::warn!("🎯 API请求构建完成 - 准备发送给模型: {}", self.model);

        Ok(ChatRequest {
            body: payload,
            headers,
        })
    }
}

fn validate_tool_calls_sequence(messages: &[Value]) -> Result<(), ApiError> {
    // Skip the system message (index 0)
    let mut i = 1;
    while i < messages.len() {
        let msg = &messages[i];
        if let Some(role) = msg.get("role").and_then(Value::as_str)
            && role == "assistant"
            && let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array)
        {
            // Collect all tool_call_ids from this assistant message
            let mut expected_call_ids: Vec<&str> = Vec::new();
            for tool_call in tool_calls {
                if let Some(call_id) = tool_call.get("id").and_then(Value::as_str) {
                    expected_call_ids.push(call_id);
                }
            }

            if !expected_call_ids.is_empty() {
                // Check that the next messages are tool messages with matching call_ids
                let mut found_call_ids = std::collections::HashSet::new();
                let mut j = i + 1;
                let mut saw_non_tool_message = false;

                while j < messages.len() {
                    let next_msg = &messages[j];
                    if let Some(next_role) = next_msg.get("role").and_then(Value::as_str) {
                        if next_role == "tool" {
                            if let Some(call_id) =
                                next_msg.get("tool_call_id").and_then(Value::as_str)
                            {
                                found_call_ids.insert(call_id);
                                j += 1;
                                continue;
                            }
                        } else {
                            // Stop at next non-tool message (assistant, user, or system)
                            saw_non_tool_message = true;
                            break;
                        }
                    }
                    j += 1;
                }

                // Only validate if we saw a non-tool message after the tool messages
                // This means the tool call sequence is complete and should have all responses
                // If we reached the end without seeing a non-tool message, this might be the
                // start of the current request (tool calls without responses yet)
                if saw_non_tool_message {
                    // Check if all expected call_ids have corresponding tool messages
                    for call_id in &expected_call_ids {
                        if !found_call_ids.contains(call_id) {
                            return Err(ApiError::Api {
                                status: StatusCode::BAD_REQUEST,
                                message: format!(
                                    "Missing tool message for tool_call_id: {call_id}. An assistant message with 'tool_calls' must be followed by tool messages responding to each 'tool_call_id'.",
                                ),
                            });
                        }
                    }
                }
                // If we didn't see a non-tool message, this might be the last message
                // (start of current request), so skip validation
            }
        } else if let Some(role) = msg.get("role").and_then(Value::as_str)
            && role == "tool"
        {
            // Allow tool messages that don't have corresponding tool_calls.
            // This can happen when receiving responses from external APIs (like DeepSeek)
            // that include tool messages without the preceding tool_calls in the conversation history.
            // These tool messages are valid as they represent completed tool executions
            // from previous interactions.
            continue;
        }
        i += 1;
    }
    Ok(())
}

fn push_tool_call_message(messages: &mut Vec<Value>, tool_call: Value, reasoning: Option<&str>) {
    // Chat Completions requires that tool calls are grouped into a single assistant message
    // (with `tool_calls: [...]`) followed by tool role responses.
    if let Some(Value::Object(obj)) = messages.last_mut()
        && obj.get("role").and_then(Value::as_str) == Some("assistant")
        && obj.get("content").is_some_and(Value::is_null)
        && let Some(tool_calls) = obj.get_mut("tool_calls").and_then(Value::as_array_mut)
    {
        tool_calls.push(tool_call);
        // DeepSeek Reasoner requires reasoning_content field when tool_calls are present
        if let Some(reasoning) = reasoning {
            if let Some(Value::String(existing)) = obj.get_mut("reasoning_content") {
                if !existing.is_empty() {
                    existing.push('\n');
                }
                existing.push_str(reasoning);
            } else {
                obj.insert(
                    "reasoning_content".to_string(),
                    Value::String(reasoning.to_string()),
                );
            }
        } else if !obj.contains_key("reasoning_content") {
            // Ensure reasoning_content exists even if empty (required by DeepSeek Reasoner)
            obj.insert(
                "reasoning_content".to_string(),
                Value::String(String::new()),
            );
        }
        return;
    }

    let mut msg = json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [tool_call],
    });
    if let Some(obj) = msg.as_object_mut() {
        // DeepSeek Reasoner requires reasoning_content field when tool_calls are present
        if let Some(reasoning) = reasoning {
            obj.insert("reasoning_content".to_string(), json!(reasoning));
        } else {
            obj.insert(
                "reasoning_content".to_string(),
                Value::String(String::new()),
            );
        }
    }
    messages.push(msg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::RetryConfig;
    use crate::provider::WireApi;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;
    use std::time::Duration;

    fn provider() -> Provider {
        Provider {
            name: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            query_params: None,
            wire: WireApi::Chat,
            headers: HeaderMap::new(),
            retry: RetryConfig {
                max_attempts: 1,
                base_delay: Duration::from_millis(10),
                retry_429: false,
                retry_5xx: true,
                retry_transport: true,
            },
            stream_idle_timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn attaches_conversation_and_subagent_headers() {
        let prompt_input = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "hi".to_string(),
            }],
        }];
        let req = ChatRequestBuilder::new("gpt-test", "inst", &prompt_input, &[], None)
            .conversation_id(Some("conv-1".into()))
            .session_source(Some(SessionSource::SubAgent(SubAgentSource::Review)))
            .build(&provider())
            .expect("request");

        assert_eq!(
            req.headers.get("session_id"),
            Some(&HeaderValue::from_static("conv-1"))
        );
        assert_eq!(
            req.headers.get("x-openai-subagent"),
            Some(&HeaderValue::from_static("review"))
        );
    }

    #[test]
    fn groups_consecutive_tool_calls_into_a_single_assistant_message() {
        let prompt_input = vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "read these".to_string(),
                }],
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "read_file".to_string(),
                arguments: r#"{"path":"a.txt"}"#.to_string(),
                call_id: "call-a".to_string(),
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "read_file".to_string(),
                arguments: r#"{"path":"b.txt"}"#.to_string(),
                call_id: "call-b".to_string(),
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "read_file".to_string(),
                arguments: r#"{"path":"c.txt"}"#.to_string(),
                call_id: "call-c".to_string(),
            },
            ResponseItem::FunctionCallOutput {
                call_id: "call-a".to_string(),
                output: FunctionCallOutputPayload {
                    content: "A".to_string(),
                    ..Default::default()
                },
            },
            ResponseItem::FunctionCallOutput {
                call_id: "call-b".to_string(),
                output: FunctionCallOutputPayload {
                    content: "B".to_string(),
                    ..Default::default()
                },
            },
            ResponseItem::FunctionCallOutput {
                call_id: "call-c".to_string(),
                output: FunctionCallOutputPayload {
                    content: "C".to_string(),
                    ..Default::default()
                },
            },
        ];

        let req = ChatRequestBuilder::new("gpt-test", "inst", &prompt_input, &[], None)
            .build(&provider())
            .expect("request");

        let messages = req
            .body
            .get("messages")
            .and_then(|v| v.as_array())
            .expect("messages array");
        // system + user + assistant(tool_calls=[...]) + 3 tool outputs
        assert_eq!(messages.len(), 6);

        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");

        let tool_calls_msg = &messages[2];
        assert_eq!(tool_calls_msg["role"], "assistant");
        assert_eq!(tool_calls_msg["content"], serde_json::Value::Null);
        let tool_calls = tool_calls_msg["tool_calls"]
            .as_array()
            .expect("tool_calls array");
        assert_eq!(tool_calls.len(), 3);
        assert_eq!(tool_calls[0]["id"], "call-a");
        assert_eq!(tool_calls[1]["id"], "call-b");
        assert_eq!(tool_calls[2]["id"], "call-c");

        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call-a");
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[4]["tool_call_id"], "call-b");
        assert_eq!(messages[5]["role"], "tool");
        assert_eq!(messages[5]["tool_call_id"], "call-c");
    }
}
