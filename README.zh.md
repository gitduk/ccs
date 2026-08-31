# CCS - Claude Code Switch

中文 | [English](README.md)

轻量级 API 代理，支持在多个提供商之间路由 Claude Code 流量，并自动完成 Anthropic ↔ OpenAI 格式转换。

## 功能特性

- **多提供商支持**：配置并切换多个 API 提供商
- **双端点**：同时服务 Anthropic（`/v1/messages`）和 OpenAI（`/v1/chat/completions`、`/v1/responses`）客户端，无需手动切换
- **格式转换**：Anthropic 与 OpenAI API 格式双向自动转换
- **流式响应**：完整支持 Server-Sent Events (SSE) 流式输出
- **TUI 管理界面**：交互式终端 UI，用于提供商配置管理
- **热重载**：无需重启即可从磁盘重载配置（TUI 按 `r`）
- **模型路由**：每个提供商可配置 glob 规则，在请求发往上游前改写模型名
- **模型映射**：每个提供商可自定义精确的模型名称映射
- **工具调用**：两种格式均完整支持 function/tool calling
- **扩展思考**：支持 Claude 的 thinking/reasoning 块
- **故障转移 / 负载均衡**：按提供商开关参与共享池，`F` 切换 Fallback（故障转移）与 LoadBalance（轮询均衡）两种模式
- **按项目路由**：为提供商分配独立监听端口，让不同项目同时使用不同提供商
- **格式自动探测**：添加提供商时自动探测 `api_format` / `api_version`，并拉取模型列表
- **连通性测试**：在模型面板中对任一模型按 `t` 测试，结果（延迟、认证状态或错误）直接显示在模型名后面
- **用量统计与请求日志**：按提供商 / 模型统计 token 用量，并可浏览请求日志，均持久化在 SQLite 中
- **额度命令**：为提供商挂一条 shell 命令，在表格的 Quota 列展示剩余额度

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
- 浏览模型列表与请求日志
- 启动 / 停止代理服务器

添加提供商时只需填写 **Name**、**Base URL**、**API Key**，保存时会自动探测 API 格式。若探测失败，表单会保留并提示：可修正 Base URL / API Key 后按 `q` 重试，或按 `a`（Anthropic）/ `o`（OpenAI）手动指定格式保存。

### 2. 或直接启动代理

```bash
ccs serve --listen 127.0.0.1:7896
```

`--listen` 可省略，省略时使用配置文件中的 `listen`。

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

配置文件路径：`~/.ccs/config.json`。设置 `CCS_CONFIG_DIR` 可改用其他目录（`$CCS_CONFIG_DIR/config.json`）。文件以原子写入方式保存，权限为 `0600`。

```json
{
  "current": "anthropic-official",
  "listen": "127.0.0.1:7896",
  "mode": "fallback",
  "request_log_limit": 100,
  "providers": {
    "anthropic-official": {
      "id": "0f0b1e9e-…",
      "base_url": "https://api.anthropic.com",
      "api_key": "$ANTHROPIC_API_KEY",
      "api_format": "anthropic",
      "enabled": true,
      "join": true,
      "model_map": {},
      "routes": []
    },
    "openrouter": {
      "id": "6d2c4a11-…",
      "base_url": "https://openrouter.ai/api",
      "api_key": "$OPENROUTER_API_KEY",
      "api_format": "openai",
      "api_version": "responses",
      "enabled": true,
      "join": false,
      "model_map": {
        "claude-sonnet-4-20250514": "anthropic/claude-sonnet-4-20250514"
      },
      "routes": [
        {
          "id": "b7f0…",
          "pattern": "*opus*",
          "target": "anthropic/claude-opus-4",
          "enabled": true
        }
      ]
    }
  }
}
```

### 顶层字段
| 字段 | 默认值 | 含义 |
| --- | --- | --- |
| `current` | — | 当前提供商名称 |
| `listen` | `127.0.0.1:7896` | 全局监听地址 |
| `providers` | — | 有序的「提供商名 → 提供商」映射 |
| `mode` | `fallback` | 提供商池的使用模式：`fallback`（故障转移）或 `load_balance`（轮询均衡） |
| `db_path` | `~/.ccs/ccs.db` | 保存统计、请求日志、模型列表的 SQLite 文件 |
| `request_log_limit` | `100` | TUI 保留的最近请求条数 |

### 提供商字段

| 字段 | 默认值 | 含义 |
| --- | --- | --- |
| `id` | 自动生成 | 稳定 UUID，重命名后不变（数据库以此为键） |
| `base_url` | — | 上游 Base URL |
| `api_key` | — | 明文 Key，或 `$ENV_VAR` 从环境变量读取 |
| `api_format` | 自动探测 | `anthropic` 或 `openai` |
| `api_version` | `responses` | 仅 OpenAI 格式生效：`responses` 或 `chat_completions` |
| `enabled` | `true` | 禁用后转发时跳过该提供商 |
| `join` | `true` | 是否参与共享提供商池（fallback 轮询或负载均衡轮询；TUI 新建时写入 `false`） |
| `routes` | `[]` | glob 路由规则（见下文） |
| `inject_thinking_history` | `true` | 向历史 assistant 轮注入空 thinking 块，DeepSeek 兼容上游需要 |
| `port` | 未设置 | 该提供商独占的 pinned 监听端口（见下文） |
| `test_model` | 未设置 | 固定用于连通性测试的模型（在模型面板中按 `m` → `p` 设置） |
| `quota_command` | 未设置 | Quota 列背后的 shell 命令 |

### API Key 解析

- 明文：`"api_key": "sk-ant-..."`
- 环境变量：`"api_key": "$ANTHROPIC_API_KEY"`（从环境变量读取）

### 模型路由

`routes` 按提供商改写请求中的模型名。第一条 **启用** 且 `pattern` 命中请求 `model` 的规则生效，其 `target` 会发往上游；`target` 为空表示「只匹配、不改写」。通配符只有 `*`，匹配任意长度的字符序列。

```json
"routes": [
  { "id": "…", "pattern": "claude-sonnet*", "target": "kimi-k2-code", "enabled": true },
  { "id": "…", "pattern": "*opus*",         "target": "deepseek-v4-pro", "enabled": true }
]
```

routes 在 `model_map` 之前应用，且按提供商各自生效：请求转移到另一个提供商时，应用的是 **那个** 提供商的 routes。routes 不参与提供商选择——提供商由 `current`、共享池（fallback / load_balance）或 pinned 端口决定。

在 TUI 编辑器的 **Routes** 区域编辑（选中提供商按 `e`，再用 Tab 移到 Routes）。

### 模型映射

精确名称映射，在 routes 之后应用：

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

### 故障转移与负载均衡（共享提供商池）

`mode` 决定请求在共享池（所有 `enabled` 且 `join=true` 的提供商，外加 `current`）上的分发方式：

- **`fallback`（故障转移）**：请求始终先打到 `current`，失败时按存储顺序依次尝试池中其他提供商。
- **`load_balance`（负载均衡）**：每个请求在池内轮询（round-robin）选起点，请求被均匀分发到各提供商；单个提供商失败时仍会继续尝试池内下一个。

`join` 字段控制提供商是否进入共享池（TUI 中选中按 `f` 切换），当前提供商始终参与。TUI 新建的提供商初始为 `false`；手写进配置文件而省略该字段时默认为 `true`。


```json
"mode": "fallback",
"providers": {
  "anthropic-official": { "join": true },
  "openrouter": { "join": true }
}
```

需要同时使用多个提供商（例如项目 A 用 OpenRouter，项目 B 用 Anthropic 直连）时，可以给每个提供商指定独立监听端口——在 TUI 中选中提供商按 `o`（留空即清除），或直接编辑配置文件：

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

ccs 启动后会额外监听 `:7901` 和 `:7902`，每个端口的请求 **固定路由** 到对应提供商，不参与共享池（fallback / load_balance）。

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

### 额度命令

`quota_command` 是一条用 `sh -lc` 执行的 shell 片段（15 秒超时），输出显示在表格的 Quota 列和额度面板中。执行时会导出 `$_API_KEY`、`$_BASE_URL`、`$_PROVIDER`，因此密钥不必明文写进命令里。在 TUI 中按 `U` 配置，或按 `u` 直接执行并刷新 Quota。

```json
"quota_command": "xh -b GET \"$_BASE_URL/v1/credits\" \"Authorization: Bearer $_API_KEY\""
```

## TUI 快捷键

### 提供商列表（Normal 模式）

| 按键 | 操作 |
| --- | --- |
| `↑/↓`、`j/k` | 上下选择提供商 |
| `gg` / `G` | 跳到顶部 / 底部 |
| `K` / `J` | 在列表中上移 / 下移提供商 |
| `s` | 切换到选中提供商 |
| `a` | 添加新提供商 |
| `e` / `Enter` | 编辑选中提供商 |
| `dd` | 删除选中提供商 |
| `p` | 切换提供商启用 / 禁用 |
| `f` | 切换该提供商的共享池参与状态（join） |
| `F` | 切换模式：Fallback（故障转移）/ LoadBalance（负载均衡） |
| `o` | 设置 / 清除该提供商的 pinned 端口 |
| `u` | 立即执行该提供商的额度命令并刷新 Quota |
| `U` | 配置该提供商的额度命令（打开 Quota Command Preview 面板） |
| `yy` | 复制该提供商的 Base URL 到剪贴板 |
| `yc` | 复制可直接运行的测试 curl 命令到剪贴板 |
| `S` | 切换后台代理服务器 |
| `r` | 从磁盘重载配置 |
| `c` | 清除当前提供商的用量数据 |
| `C` | 清除所有提供商的用量数据 |
| `l` | 打开请求日志面板 |
| `m` | 打开模型面板 |
| `h` / `?` | 显示帮助 |
| `Ctrl-L` | 清除消息日志 |
| `q` / `Esc` | 退出（后台代理未运行时需确认） |

复制功能依赖 `wl-copy`（Wayland）。

### 提供商编辑器（`a` / `e`）

字段依次为 Name、Base URL、API Key，以及 Routes 区域。Vim 风格，默认处于 Normal 模式。

| 按键 | 操作 |
| --- | --- |
| `i` / `a` | 进入 Insert 模式 |
| `Esc` | Insert → Normal |
| `q` / `Esc`（Normal） | 保存并关闭 |
| `j` / `k` | 上一个 / 下一个字段 |
| `Tab` / `Shift-Tab` | 下一个 / 上一个字段（循环经过 Routes） |
| `h` / `l`、`0` / `$` | 在字段内移动光标 |

### 路由规则（编辑器的 Routes 区域内）

| 按键 | 操作 |
| --- | --- |
| `j` / `k` | 在规则间移动（到边界时离开该区域） |
| `a` / `o` | 新建规则（自动进入 pattern 的 Insert 模式） |
| `i` / `Enter` | 编辑选中规则的 pattern |
| `t` | 编辑选中规则的 target（带模型补全） |
| `Space` | 切换规则启用 / 禁用 |
| `dd` | 删除选中规则 |
| `K` / `J` | 上移 / 下移规则（调整优先级） |
| `Esc` | Insert → Normal |

### 请求日志面板（`l`）

| 按键 | 操作 |
| --- | --- |
| `j` / `k`、`↑/↓` | 选择请求 |
| `J` / `K` | 详情区域上下滚动半页 |
| `gg` / `G`、`Home` / `End` | 详情区域跳到顶部 / 底部 |
| `q` / `Esc` | 返回提供商列表 |

### 模型面板（`m`）

| 按键 | 操作 |
| --- | --- |
| `j` / `k`、`↑/↓` | 上下移动 |
| `Ctrl-D` / `Ctrl-U`、`PgDn` / `PgUp` | 跳动 10 条 |
| `gg` / `G` | 跳到顶部 / 底部 |
| `i` | 进入过滤输入（Insert），`Esc` 退出过滤 |
| `Ctrl-J` / `Ctrl-K` | 过滤状态下上下移动 |
| `yy` / `Enter` | 复制选中的模型名 |
| `q` / `Esc` / `Ctrl-C` | 返回提供商列表 |
| `t` | 测试光标下的模型（结果显示在模型名后） |
| `p` | 设置 / 清除该提供商的 Test Model |

### 额度面板（`U`）

| 按键 | 操作 |
| --- | --- |
| `i` / `a` | 编辑命令（Insert） |
| `s` | 执行命令并预览输出 |
| `j` / `k` | 滚动预览 |
| `Ctrl-L`（Insert） | 清空命令 |
| `q` / `Esc` | 保存并关闭 |

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
  "version": "0.45.0"
}
```

配置重载在 TUI 中按 `r` 完成，没有对应的 HTTP 端点。

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
- 配置文件以 `0600` 权限写入
- 提供商表格中 API Key 始终遮码，编辑器中未聚焦时也遮码
- 额度命令通过 `$_API_KEY` 拿到密钥，无需存进命令文本
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
