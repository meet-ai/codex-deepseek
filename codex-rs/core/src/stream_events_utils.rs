use std::pin::Pin;
use std::sync::Arc;

use codex_protocol::items::TurnItem;
use tokio_util::sync::CancellationToken;

use crate::codex::Session;
use crate::codex::TurnContext;
use crate::error::CodexErr;
use crate::error::Result;
use crate::function_tool::FunctionCallError;
use crate::parse_turn_item;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::router::ToolRouter;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use futures::Future;
use tracing::debug;
use tracing::instrument;

/// Handle a completed output item from the model stream, recording it and
/// queuing any tool execution futures. This records items immediately so
/// history and rollout stay in sync even if the turn is later cancelled.
pub(crate) type InFlightFuture<'f> =
    Pin<Box<dyn Future<Output = Result<ResponseInputItem>> + Send + 'f>>;

#[derive(Default)]
pub(crate) struct OutputItemResult {
    pub last_agent_message: Option<String>,
    pub needs_follow_up: bool,
    pub tool_future: Option<InFlightFuture<'static>>,
}

pub(crate) struct HandleOutputCtx {
    pub sess: Arc<Session>,
    pub turn_context: Arc<TurnContext>,
    pub tool_runtime: ToolCallRuntime,
    pub cancellation_token: CancellationToken,
}

#[instrument(level = "trace", skip_all)]
pub(crate) async fn handle_output_item_done(
    ctx: &mut HandleOutputCtx,
    item: ResponseItem,
    previously_active_item: Option<TurnItem>,
) -> Result<OutputItemResult> {
    let mut output = OutputItemResult::default();

    match ToolRouter::build_tool_call(ctx.sess.as_ref(), item.clone()).await {
        // The model emitted a tool call; log it, persist the item immediately, and queue the tool execution.
        Ok(Some(call)) => {
            let payload_preview = call.payload.log_payload().into_owned();
            tracing::warn!(
                "🔧 工具调用: {} {} (call_id: {})",
                call.tool_name,
                payload_preview,
                call.call_id
            );

            ctx.sess
                .record_conversation_items(&ctx.turn_context, std::slice::from_ref(&item))
                .await;

            let cancellation_token = ctx.cancellation_token.child_token();
            let tool_future: InFlightFuture<'static> = Box::pin(
                ctx.tool_runtime
                    .clone()
                    .handle_tool_call(call, cancellation_token),
            );

            output.needs_follow_up = true;
            output.tool_future = Some(tool_future);
        }
        // No tool call: convert messages/reasoning into turn items and mark them as complete.
        Ok(None) => {
            // 处理DeepSeek reasoning_content
            if let ResponseItem::Message { role, reasoning_content, .. } = &item {
                if role == "assistant" {
                    if let Some(reasoning) = reasoning_content {
                        tracing::warn!("🧠 存储DeepSeek reasoning_content到session (长度: {})", reasoning.len());
                        ctx.sess.set_reasoning_content(reasoning.clone()).await;
                    }
                }
            }

            if let Some(turn_item) = handle_non_tool_response_item(&item).await {
                if previously_active_item.is_none() {
                    ctx.sess
                        .emit_turn_item_started(&ctx.turn_context, &turn_item)
                        .await;
                }

                ctx.sess
                    .emit_turn_item_completed(&ctx.turn_context, turn_item)
                    .await;
            }

            ctx.sess
                .record_conversation_items(&ctx.turn_context, std::slice::from_ref(&item))
                .await;
            let last_agent_message = last_assistant_message_from_item(&item);

            output.last_agent_message = last_agent_message;
        }
        // Guardrail: the model issued a LocalShellCall without an id; surface the error back into history.
        Err(FunctionCallError::MissingLocalShellCallId) => {
            let msg = "LocalShellCall without call_id or id";
            ctx.turn_context
                .client
                .get_otel_manager()
                .log_tool_failed("local_shell", msg);
            tracing::error!(msg);

            let response = ResponseInputItem::FunctionCallOutput {
                call_id: String::new(),
                output: FunctionCallOutputPayload {
                    content: msg.to_string(),
                    ..Default::default()
                },
            };
            ctx.sess
                .record_conversation_items(&ctx.turn_context, std::slice::from_ref(&item))
                .await;
            if let Some(response_item) = response_input_to_response_item(&response) {
                ctx.sess
                    .record_conversation_items(
                        &ctx.turn_context,
                        std::slice::from_ref(&response_item),
                    )
                    .await;
            }

            output.needs_follow_up = true;
        }
        // The tool request should be answered directly (or was denied); push that response into the transcript.
        Err(FunctionCallError::RespondToModel(message)) => {
            let response = ResponseInputItem::FunctionCallOutput {
                call_id: String::new(),
                output: FunctionCallOutputPayload {
                    content: message,
                    ..Default::default()
                },
            };
            ctx.sess
                .record_conversation_items(&ctx.turn_context, std::slice::from_ref(&item))
                .await;
            if let Some(response_item) = response_input_to_response_item(&response) {
                ctx.sess
                    .record_conversation_items(
                        &ctx.turn_context,
                        std::slice::from_ref(&response_item),
                    )
                    .await;
            }

            output.needs_follow_up = true;
        }
        // A fatal error occurred; surface it back into history.
        Err(FunctionCallError::Fatal(message)) => {
            return Err(CodexErr::Fatal(message));
        }
    }

    Ok(output)
}

pub(crate) async fn handle_non_tool_response_item(item: &ResponseItem) -> Option<TurnItem> {
    debug!(?item, "Output item");

    match item {
        ResponseItem::Message { content, role, reasoning_content, .. } => {
            // 记录助手消息内容
            let message_text = content
                .iter()
                .filter_map(|ci| match ci {
                    codex_protocol::models::ContentItem::OutputText { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if !message_text.is_empty() && message_text.len() > 50 {
                tracing::warn!("💭 助手回复: {}...", &message_text[..50]);
            } else if !message_text.is_empty() {
                tracing::warn!("💭 助手回复: {}", message_text);
            }

            // 处理DeepSeek reasoning_content
            if role == "assistant" {
                if let Some(reasoning) = reasoning_content {
                    tracing::warn!("🧠 存储DeepSeek reasoning_content (长度: {})", reasoning.len());
                    // 这里我们需要访问session来存储reasoning_content
                    // 但这个函数没有session参数，所以我们需要在调用处处理
                }
            }

            parse_turn_item(item)
        }
        ResponseItem::Reasoning { content, .. } => {
            // 记录推理过程
            if let Some(content_items) = content {
                let reasoning_parts: Vec<String> = content_items
                    .iter()
                    .filter_map(|ci| match ci {
                        codex_protocol::models::ReasoningItemContent::ReasoningText { text } => {
                            Some(format!("推理: {}", text))
                        }
                        codex_protocol::models::ReasoningItemContent::Text { text } => {
                            Some(format!("文本: {}", text))
                        }
                    })
                    .collect();

                tracing::warn!("🧠 LLM 推理内容: {} 个片段", reasoning_parts.len());

                for (i, part) in reasoning_parts.iter().enumerate() {
                    if part.len() > 200 {
                        tracing::warn!("🧠 推理片段 {}: {}...", i + 1, &part[..200]);
                    } else {
                        tracing::warn!("🧠 推理片段 {}: {}", i + 1, part);
                    }
                }
            }
            parse_turn_item(item)
        }
        ResponseItem::WebSearchCall { action, .. } => {
            match action {
                codex_protocol::models::WebSearchAction::Search { query } => {
                    if let Some(q) = query {
                        tracing::warn!("🔍 网络搜索: {}", q);
                    }
                }
                codex_protocol::models::WebSearchAction::OpenPage { url } => {
                    if let Some(u) = url {
                        tracing::warn!("🔗 打开网页: {}", u);
                    }
                }
                codex_protocol::models::WebSearchAction::FindInPage { url, pattern } => {
                    if let Some(u) = url {
                        if let Some(p) = pattern {
                            tracing::warn!("🔍 在页面中查找: {} -> {}", u, p);
                        } else {
                            tracing::warn!("🔍 访问页面: {}", u);
                        }
                    }
                }
                codex_protocol::models::WebSearchAction::Other => {
                    tracing::warn!("🔍 其他网络操作");
                }
            }
            parse_turn_item(item)
        }
        ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. } => {
            debug!("unexpected tool output from stream");
            None
        }
        _ => None,
    }
}

pub(crate) fn last_assistant_message_from_item(item: &ResponseItem) -> Option<String> {
    if let ResponseItem::Message { role, content, .. } = item
        && role == "assistant"
    {
        return content.iter().rev().find_map(|ci| match ci {
            codex_protocol::models::ContentItem::OutputText { text } => Some(text.clone()),
            _ => None,
        });
    }
    None
}

pub(crate) fn response_input_to_response_item(input: &ResponseInputItem) -> Option<ResponseItem> {
    match input {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            Some(ResponseItem::FunctionCallOutput {
                call_id: call_id.clone(),
                output: output.clone(),
            })
        }
        ResponseInputItem::CustomToolCallOutput { call_id, output } => {
            Some(ResponseItem::CustomToolCallOutput {
                call_id: call_id.clone(),
                output: output.clone(),
            })
        }
        ResponseInputItem::McpToolCallOutput { call_id, result } => {
            let output = match result {
                Ok(call_tool_result) => FunctionCallOutputPayload::from(call_tool_result),
                Err(err) => FunctionCallOutputPayload {
                    content: err.clone(),
                    success: Some(false),
                    ..Default::default()
                },
            };
            Some(ResponseItem::FunctionCallOutput {
                call_id: call_id.clone(),
                output,
            })
        }
        _ => None,
    }
}
