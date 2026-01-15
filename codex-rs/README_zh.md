# Codex CLI (Rust 实现)

Codex CLI 是一个基于 Rust 的智能编码助手，提供零依赖的本地可执行文件。本项目是针对 DeepSeek 模型进行优化的 Codex CLI 分支。

## 项目概述

Codex CLI 是一个终端环境下的 AI 代码助手，能够理解自然语言指令并执行代码相关任务，如文件编辑、命令执行、代码重构等。它采用沙盒机制确保安全性，支持多种模型后端，并提供了丰富的配置选项。

### 针对 DeepSeek 模型的优化

此分支专门针对 DeepSeek 模型进行了以下关键优化：

1. **移除 developer 角色支持**：将协议中的 "developer" 角色转换为 "user" 角色，因为 Chat Completions API 不支持 "developer" 角色。
2. **修复 reasoning_content 字段问题**：确保当 tool_calls 存在时，即使为空也包含 reasoning_content 字段，以满足 DeepSeek Reasoner 的要求。

具体修改位于 `codex-api/src/requests/chat.rs`：

- **第 155-160 行**：将 "developer" 角色转换为 "user" 角色
- **第 329-345 行**：添加 reasoning_content 字段支持
- **第 358-365 行**：确保推理内容正确序列化

## 安装 Codex

目前，最简单的安装方式是通过 `npm`：

```shell
npm i -g @openai/codex
codex
```

也可以通过 Homebrew 安装 (`brew install --cask codex`) 或直接从 [GitHub Releases](https://github.com/openai/codex/releases) 下载平台特定的版本。

## 快速开始

- 首次使用 Codex？请从 [`docs/getting-started.md`](../docs/getting-started.md) 开始（包含提示、快捷键和会话管理的完整指南）。
- 需要更深入的控制？请查看 [`docs/config.md`](../docs/config.md) 和 [`docs/install.md`](../docs/install.md)。

## Rust CLI 的新特性

Rust 实现现在是维护中的 Codex CLI，提供默认体验。它包含许多旧版 TypeScript CLI 不支持的特性。

### 配置

Codex 支持丰富的配置选项。请注意 Rust CLI 使用 `config.toml` 而不是 `config.json`。详情请参阅 [`docs/config.md`](../docs/config.md)。

### 模型上下文协议支持

#### MCP 客户端

Codex CLI 作为 MCP 客户端，允许 Codex CLI 和 IDE 扩展在启动时连接到 MCP 服务器。详情请参阅[配置文档](../docs/config.md#connecting-to-mcp-servers)。

#### MCP 服务器（实验性）

通过运行 `codex mcp-server`，Codex 可以启动为 MCP _服务器_。这允许其他 MCP 客户端将 Codex 作为另一个代理的工具使用。

使用 [`@modelcontextprotocol/inspector`](https://github.com/modelcontextprotocol/inspector) 尝试：

```shell
npx @modelcontextprotocol/inspector codex mcp-server
```

使用 `codex mcp` 来添加/列出/获取/删除在 `config.toml` 中定义的 MCP 服务器启动器，使用 `codex mcp-server` 直接运行 MCP 服务器。

### 通知

您可以配置脚本，在代理完成每次回合时运行，从而启用通知功能。[通知文档](../docs/config.md#notify) 包含详细示例，说明如何在 macOS 上通过 [terminal-notifier](https://github.com/julienXX/terminal-notifier) 获取桌面通知。当 Codex 检测到它在 Windows Terminal 中的 WSL 2 下运行（设置了 `WT_SESSION`）时，TUI 会自动回退到原生 Windows toast 通知，因此即使 Windows Terminal 未实现 OSC 9，批准提示和完成的回合也会显示。

### 通过 `codex exec` 以编程方式/非交互方式运行 Codex

要以非交互方式运行 Codex，请运行 `codex exec PROMPT`（也可以通过 `stdin` 传递提示），Codex 将处理您的任务，直到它认为完成并退出。输出直接打印到终端。您可以设置 `RUST_LOG` 环境变量以查看更多信息。

### 实验 Codex 沙盒

要测试在 Codex 提供的沙盒下运行命令时会发生什么，我们提供了以下 Codex CLI 子命令：

```
# macOS
codex sandbox macos [--full-auto] [--log-denials] [COMMAND]...

# Linux
codex sandbox linux [--full-auto] [COMMAND]...

# Windows
codex sandbox windows [--full-auto] [COMMAND]...

# 旧版别名
codex debug seatbelt [--full-auto] [--log-denials] [COMMAND]...
codex debug landlock [--full-auto] [COMMAND]...
```

### 通过 `--sandbox` 选择沙盒策略

Rust CLI 提供了一个专用的 `--sandbox` (`-s`) 标志，让您可以选择沙盒策略，而无需使用通用的 `-c/--config` 选项：

```shell
# 使用默认的只读沙盒运行 Codex
codex --sandbox read-only

# 允许代理在当前工作区内写入，同时阻止网络访问
codex --sandbox workspace-write

# 危险！完全禁用沙盒（仅在已经在容器或其他隔离环境中运行时才这样做）
codex --sandbox danger-full-access
```

相同的设置可以通过顶层 `sandbox_mode = "MODE"` 键持久保存在 `~/.codex/config.toml` 中，例如 `sandbox_mode = "workspace-write"`。

## 代码组织

此文件夹是 Cargo 工作区的根目录。它包含相当多的实验性代码，但以下是关键组件：

### 核心模块

- **`core/`** - 包含 Codex 的业务逻辑。我们希望这成为一个库 crate，对于构建其他使用 Codex 的 Rust/本机应用程序非常有用。
- **`cli/`** - 提供上述 CLI 子命令的 CLI 多功能工具。
- **`exec/`** - 用于自动化的“无头” CLI。
- **`tui/`** - 启动使用 [Ratatui](https://ratatui.rs/) 构建的全屏 TUI 的 CLI。
- **`tui2/`** - 新一代 TUI 界面。
- **`codex-api/`** - 与 AI 模型 API 通信的核心模块，包含针对 DeepSeek 的修改。
- **`protocol/`** - 定义客户端和服务器之间通信协议的类型和结构。

### 支持模块

- **`mcp-server/`** - Model Context Protocol 服务器实现
- **`mcp-types/`** - MCP 类型定义
- **`ollama/`** - Ollama 模型支持
- **`lmstudio/`** - LM Studio 模型支持
- **`chatgpt/`** - ChatGPT 模型支持
- **`execpolicy/`** - 执行策略管理
- **`linux-sandbox/`** - Linux 沙盒实现
- **`windows-sandbox-rs/`** - Windows 沙盒实现

### 实用工具

- **`utils/`** - 各种实用工具函数
- **`common/`** - 共享类型和常量
- **`apply-patch/`** - 补丁应用功能
- **`ansi-escape/`** - ANSI 转义序列处理
- **`file-search/`** - 文件搜索功能

## 开发指南

### 构建项目

```shell
cargo build --release
```

### 运行测试

```shell
cargo test
```

### 代码格式化

```shell
just fmt
```

### 代码修复

```shell
just fix -p <project>
```

## 许可证

本项目基于 Codex CLI 的开源实现，针对 DeepSeek 模型进行了优化。

## 贡献

欢迎提交问题和拉取请求。对于重大更改，请先打开 issue 讨论您想要更改的内容。

## 致谢

- OpenAI Codex 团队 - 原始 Codex CLI 项目
- DeepSeek - AI 模型提供商
- Rust 社区 - 优秀的工具和库生态系统
