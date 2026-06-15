# CCS - Claude Code Switch

中文 | [English](README.md)

轻量级 API 代理，支持在多个提供商之间路由 Claude Code 流量，并自动完成 Anthropic ↔ OpenAI 格式转换。

## 功能特性

- **多提供商支持**：配置并切换多个 API 提供商
- **双端点**：同时服务 Anthropic（`/v1/messages`）和 OpenAI（`/v1/chat/completions`、`/v1/responses`）客户端，无需手动切换
- **格式转换**：Anthropic 与 OpenAI API 格式双向自动转换
- **流式响应**：完整支持 Server-Sent Events (SSE) 流式输出
- **TUI 管理界面**：交互式终端 UI，用于提供商配置管理
- **热重载**：无需重启即可重载配置（TUI 按 `r` 或 POST `/reload`）
- **模型映射**：每个提供商可自定义模型名称映射
- **工具调用**：两种格式均完整支持 function/tool calling
- **扩展思考**：支持 Claude 的 thinking/reasoning 块
- **按提供商控制 Fallback**：每个提供商独立控制是否参与 fallback 轮询
- **自动拉取模型列表**：添加新提供商时自动获取可用模型列表
- **连通性测试**：测试提供商后，Info 面板展示状态、延迟、模型数量、工具调用支持、图片输入支持

## 截图

主界面
![Main TUI](assets/screenshot-main.png)

提供商编辑
![Edit Provider](assets/screenshot-edit.png)

请求日志界面
![Request Logs](assets/screenshot-logs.png)

模型列表界面
![Models Panel](assets/screenshot-models.png)

## 安装

```bash
cargo install --path .
```

## 快速开始

### 1. 启动 TUI

```bash
ccs
```

配置了提供商后，TUI 会自动启动代理服务器。

TUI 支持：
- 添加 / 编辑 / 删除提供商
- 切换当前提供商
- 测试连通性
- 启动 / 停止代理服务器

### 2. 或直接启动代理

```bash
ccs serve --listen 127.0.0.1:7896
```

### 3. 配置客户端

**Anthropic 客户端：**

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:7896
```

**OpenAI 客户端：**

```bash
export OPENAI_BASE_URL=http://127.0.0.1:7896
export OPENAI_API_KEY=any-value
```

两种端点共享同一个当前提供商，无需任何额外配置。

## 配置

配置文件路径：`~/.ccs/config.json`

```json
{
  "current": "anthropic-official",
  "listen": "0.0.0.0:7896",
  "providers": {
    "anthropic-official": {
      "base_url": "https://api.anthropic.com",
      "api_key": "$ANTHROPIC_API_KEY",
      "api_format": "anthropic",
      "model_map": {},
      "fallback": true
    },
    "openrouter": {
      "base_url": "https://openrouter.ai/api",
      "api_key": "$OPENROUTER_API_KEY",
      "api_format": "openai",
      "model_map": {
        "claude-sonnet-4-20250514": "anthropic/claude-sonnet-4-20250514"
      },
      "fallback": false
    }
  }
}
```

### API Key 解析

- 明文：`"api_key": "sk-ant-..."`
- 环境变量：`"api_key": "$ANTHROPIC_API_KEY"`（从环境变量读取）

### 模型映射

将 Claude 模型名映射为提供商专用名称：

```json
"model_map": {
  "claude-sonnet-4-20250514": "anthropic/claude-sonnet-4-20250514",
  "claude-opus-4-20250514": "anthropic/claude-opus-4-20250514"
}
```

### OpenAI API 版本

对于 OpenAI 格式的提供商，`api_version` 控制上游使用哪个端点：

- `"responses"`（默认）— 使用 `/v1/responses`
- `"chat_completions"` — 使用 `/v1/chat/completions`

```json
"api_version": "chat_completions"
```

### 按提供商控制 Fallback

将 `"fallback": true` 设置为参与 fallback 轮询；当前提供商失败时会依次尝试。新建提供商默认为 `false`。

```json
"fallback": true
```

### 按项目路由（多端口 Pinned Listener）

需要同时使用多个提供商（例如项目 A 用 OpenRouter，项目 B 用 Anthropic 直连）时，可以给每个提供商指定独立监听端口。

**配置方式：**

在 TUI 编辑提供商时填写 `Port` 字段（留空则不开启），或直接在 `~/.ccs/config.json` 中添加：

```json
{
  "providers": {
    "openrouter": {
      "base_url": "https://openrouter.ai/api",
      "port": 7901
    },
    "anthropic": {
      "base_url": "https://api.anthropic.com",
      "port": 7902
    }
  }
}
```

ccs 启动后会额外监听 `:7901` 和 `:7902`，每个端口的请求 **固定路由** 到对应提供商，不参与 fallback。

**项目侧配置（推荐配合 direnv）：**

```bash
# project-a/.envrc
export ANTHROPIC_BASE_URL=http://127.0.0.1:7901   # → openrouter

# project-b/.envrc
export ANTHROPIC_BASE_URL=http://127.0.0.1:7902   # → anthropic
```

**限制：**
- 各提供商端口不能冲突，也不能与全局 `listen` 端口重复
- 禁用某提供商会停止其 pinned 端口监听；重新启用会恢复
- 热重载（TUI `r` 键）会动态增删端口监听

## TUI 快捷键

### 导航
- `↑/↓` 或 `j/k` — 上下选择提供商
- `gg` / `G` — 跳到顶部 / 底部
- `K/J` — 在列表中上移 / 下移提供商

### 提供商操作
- `s` — 切换到选中提供商
- `a` / `o` — 添加新提供商
- `e` / `Enter` — 编辑选中提供商
- `dd` — 删除选中提供商
- `p` — 切换提供商启用 / 禁用
- `f` — 切换提供商 fallback 参与状态
- `F` — 切换全局 fallback 模式
- `t` — 测试连通性
- `u` — 配置选中提供商的 Quota
- `yy` — 复制选中提供商
- `yc` — 复制提供商配置到剪贴板

### 服务器 & 配置
- `S` — 切换后台代理服务器
- `r` — 从磁盘重载配置
- `c` — 清除当前提供商选择
- `C` — 清除所有提供商

### 视图
- `l` — 打开请求日志面板
- `m` — 打开模型面板
- `h` 或 `?` — 显示帮助

### 通用
- `Ctrl-L` — 清除消息日志
- `q` 或 `Esc` — 退出

## API 端点

### Anthropic 兼容

#### POST /v1/messages

接受 Anthropic Messages API 格式，自动转换为上游提供商所需格式。

#### GET /v1/models（Anthropic 格式）

请求使用 `x-api-key` 或无认证头时，返回 Anthropic 格式的模型列表。

### OpenAI 兼容

#### POST /v1/chat/completions

接受 OpenAI Chat Completions 格式，内部规范化为 Anthropic 格式，响应转换回 OpenAI 格式返回。

#### POST /v1/responses

接受 OpenAI Responses API 格式，处理流程与 `/v1/chat/completions` 相同。

#### GET /v1/models（OpenAI 格式）

请求使用 `Bearer` token 时，返回 OpenAI 格式的模型列表。

### 工具端点

#### GET /health

```json
{
  "status": "ok",
  "provider": "anthropic-official",
  "version": "0.1.0"
}
```

#### POST /reload

无需重启，从磁盘重载配置：

```bash
xh POST http://localhost:7896/reload
```

## 格式转换

### Anthropic → OpenAI

- `system` → system message
- `messages` → messages 数组
- `tool_use` → `tool_calls`
- `tool_result` → tool role message
- `thinking` 块 → `reasoning_content`
- `stop_sequences` → `stop`

### OpenAI → Anthropic

- `tool_calls` → `tool_use` 块
- `reasoning_content` → `thinking` 块
- `finish_reason` 映射：
  - `stop` → `end_turn`
  - `length` → `max_tokens`
  - `tool_calls` → `tool_use`

## 安全说明

- 以 `$` 开头的 API Key 从环境变量读取
- 建议限制配置文件权限为 0600
- TUI 中未聚焦时 API Key 自动遮码
- 错误信息不会暴露敏感内容

## 开发

### 构建

```bash
cargo build --release
```

### 运行测试

```bash
cargo test
```

### 运行 Clippy

```bash
cargo clippy --all-targets --all-features
```

## License

详见 LICENSE 文件。
