use crate::client_common::tools::ToolSpec;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::function_tool::FunctionCallError;
use crate::sandboxing::SandboxPermissions;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ConfiguredToolSpec;
use crate::tools::registry::ToolRegistry;
use crate::tools::spec::ToolsConfig;
use crate::tools::spec::build_specs;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::ShellToolCallParams;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::instrument;

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub tool_name: String,
    pub call_id: String,
    pub payload: ToolPayload,
}

pub struct ToolRouter {
    registry: ToolRegistry,
    specs: Vec<ConfiguredToolSpec>,
}

impl ToolRouter {
    pub fn from_config(
        config: &ToolsConfig,
        mcp_tools: Option<HashMap<String, mcp_types::Tool>>,
    ) -> Self {
        let builder = build_specs(config, mcp_tools);
        let (specs, registry) = builder.build();

        Self { registry, specs }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.specs
            .iter()
            .map(|config| config.spec.clone())
            .collect()
    }

    pub fn tool_supports_parallel(&self, tool_name: &str) -> bool {
        self.specs
            .iter()
            .filter(|config| config.supports_parallel_tool_calls)
            .any(|config| config.spec.name() == tool_name)
    }

    #[instrument(level = "trace", skip_all, err)]
    pub async fn build_tool_call(
        session: &Session,
        item: ResponseItem,
    ) -> Result<Option<ToolCall>, FunctionCallError> {
        match item {
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                let tool_call =
                    if let Some((server, tool)) = session.parse_mcp_tool_name(&name).await {
                        tracing::warn!(
                            "🔍 解析工具调用: MCP工具 {} -> 服务器: {} 工具: {} (call_id: {})",
                            name,
                            server,
                            tool,
                            call_id
                        );
                        ToolCall {
                            tool_name: name,
                            call_id,
                            payload: ToolPayload::Mcp {
                                server,
                                tool,
                                raw_arguments: arguments,
                            },
                        }
                    } else {
                        tracing::warn!(
                            "🔍 解析工具调用: 函数工具 {} (call_id: {}) 参数内容: {} (长度: {})",
                            name,
                            call_id,
                            arguments,
                            arguments.to_string().len()
                        );
                        ToolCall {
                            tool_name: name,
                            call_id,
                            payload: ToolPayload::Function { arguments },
                        }
                    };
                Ok(Some(tool_call))
            }
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => {
                tracing::warn!(
                    "🔍 解析工具调用: 自定义工具 {} (call_id: {}) 输入长度: {}",
                    name,
                    call_id,
                    input.to_string().len()
                );
                Ok(Some(ToolCall {
                    tool_name: name,
                    call_id,
                    payload: ToolPayload::Custom { input },
                }))
            }
            ResponseItem::LocalShellCall {
                id,
                call_id,
                action,
                ..
            } => {
                let final_call_id = call_id
                    .or(id)
                    .ok_or(FunctionCallError::MissingLocalShellCallId)?;

                match action {
                    LocalShellAction::Exec(exec) => {
                        tracing::warn!(
                            "🔍 解析工具调用: 本地Shell命令 {} (call_id: {}) 命令: {} 工作目录: {:?}",
                            "local_shell",
                            final_call_id,
                            exec.command.join(" "),
                            exec.working_directory
                        );
                        let params = ShellToolCallParams {
                            command: exec.command,
                            workdir: exec.working_directory,
                            timeout_ms: exec.timeout_ms,
                            sandbox_permissions: Some(SandboxPermissions::UseDefault),
                            justification: None,
                        };
                        Ok(Some(ToolCall {
                            tool_name: "local_shell".to_string(),
                            call_id: final_call_id,
                            payload: ToolPayload::LocalShell { params },
                        }))
                    }
                }
            }
            _ => Ok(None),
        }
    }

    #[instrument(level = "trace", skip_all, err)]
    pub async fn dispatch_tool_call(
        &self,
        session: Arc<Session>,
        turn: Arc<TurnContext>,
        tracker: SharedTurnDiffTracker,
        call: ToolCall,
    ) -> Result<ResponseInputItem, FunctionCallError> {
        let ToolCall {
            tool_name,
            call_id,
            payload,
        } = call;
        let payload_outputs_custom = matches!(payload, ToolPayload::Custom { .. });
        let failure_call_id = call_id.clone();
        let tool_name_clone = tool_name.clone();

        let invocation = ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
        };

        tracing::warn!(
            "📤 分发工具调用: {} (call_id: {})",
            tool_name_clone,
            failure_call_id
        );
        match self.registry.dispatch(invocation).await {
            Ok(response) => {
                tracing::warn!(
                    "📥 工具调用 {} (call_id: {}) 完成",
                    tool_name_clone,
                    failure_call_id
                );
                Ok(response)
            }
            Err(FunctionCallError::Fatal(message)) => {
                tracing::warn!(
                    "💥 工具调用 {} (call_id: {}) 发生致命错误: {}",
                    tool_name_clone,
                    failure_call_id,
                    message
                );
                Err(FunctionCallError::Fatal(message))
            }
            Err(err) => {
                tracing::warn!(
                    "⚠️ 工具调用 {} (call_id: {}) 失败: {:?}",
                    tool_name_clone,
                    failure_call_id,
                    err
                );
                Ok(Self::failure_response(
                    failure_call_id,
                    payload_outputs_custom,
                    err,
                ))
            }
        }
    }

    fn failure_response(
        call_id: String,
        payload_outputs_custom: bool,
        err: FunctionCallError,
    ) -> ResponseInputItem {
        let message = err.to_string();
        if payload_outputs_custom {
            ResponseInputItem::CustomToolCallOutput {
                call_id,
                output: message,
            }
        } else {
            ResponseInputItem::FunctionCallOutput {
                call_id,
                output: codex_protocol::models::FunctionCallOutputPayload {
                    content: message,
                    success: Some(false),
                    ..Default::default()
                },
            }
        }
    }
}
