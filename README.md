# CCS - Claude Code Switch

English | [中文](README.zh.md)

A lightweight API proxy for routing Claude Code traffic between multiple providers with Anthropic ↔ OpenAI format conversion.

## Features

- **Multi-Provider Support**: Configure and switch between multiple API providers
- **Dual-Endpoint**: Serve Anthropic (`/v1/messages`) and OpenAI (`/v1/chat/completions`, `/v1/responses`) clients simultaneously — no toggle needed
- **Format Conversion**: Automatic bidirectional translation between Anthropic and OpenAI API formats
- **Streaming Support**: Full support for Server-Sent Events (SSE) streaming responses
- **TUI Management**: Interactive terminal UI for provider configuration
- **Hot Reload**: Reload configuration without restarting the proxy (press `r` in TUI or POST to `/reload`)
- **Model Mapping**: Custom model name mapping per provider
- **Tool Calling**: Full support for function/tool calling in both formats
- **Extended Thinking**: Support for Claude's thinking/reasoning blocks
- **Per-Provider Fallback**: Each provider independently opts in/out of the fallback rotation

## Installation

```bash
cargo install --path .
```

## Quick Start

### 1. Start the TUI

```bash
ccs
```

The TUI will automatically start the proxy server if you have a provider configured.

The TUI allows you to:
- Add/edit/delete providers
- Switch between providers
- Test connectivity
- Start/stop the proxy server

### 2. Or start the proxy directly

```bash
ccs serve --listen 127.0.0.1:7896
```

### 3. Configure your client

**Anthropic clients:**

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:7896
```

**OpenAI clients:**

```bash
export OPENAI_BASE_URL=http://127.0.0.1:7896
export OPENAI_API_KEY=any-value
```

Both endpoints share the same active provider — no configuration change needed to switch clients.

## Configuration

Configuration is stored in `~/.ccs/config.json`:

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

### API Key Resolution

- Plain text: `"api_key": "sk-ant-..."`
- Environment variable: `"api_key": "$ANTHROPIC_API_KEY"` (reads from env)

### Model Mapping

Map Claude model names to provider-specific names:

```json
"model_map": {
  "claude-sonnet-4-20250514": "anthropic/claude-sonnet-4-20250514",
  "claude-opus-4-20250514": "anthropic/claude-opus-4-20250514"
}
```

### OpenAI API Version

For OpenAI-format providers, set `api_version` to control which upstream endpoint is used:

- `"responses"` (default) — use the `/v1/responses` endpoint
- `"chat_completions"` — use the `/v1/chat/completions` endpoint

```json
"api_version": "chat_completions"
```

### Per-Provider Fallback

Set `"fallback": true` to include a provider in the fallback rotation when the active provider fails. New providers default to `false`.

```json
"fallback": true
```

## TUI Keybindings

### Navigation
- `↑/↓` or `j/k` - Navigate providers
- `gg` / `G` - Go to top / bottom
- `K/J` - Move provider up / down in list

### Provider Actions
- `s` - Switch to selected provider
- `a` / `o` - Add new provider
- `e` / `Enter` - Edit selected provider
- `dd` - Delete selected provider
- `p` - Toggle provider enabled/disabled
- `f` - Toggle provider fallback participation
- `F` - Toggle global fallback mode
- `t` - Test connectivity
- `u` - Configure quota for selected provider
- `yy` - Duplicate selected provider
- `yc` - Copy provider config to clipboard

### Server & Config
- `S` - Toggle background proxy server
- `r` - Reload configuration from disk
- `c` - Clear current provider selection
- `C` - Clear all providers

### Views
- `l` - Open request logs panel
- `m` - Open models panel
- `h` or `?` - Show help

### General
- `Ctrl-L` - Clear message log
- `q` or `Esc` - Quit

## API Endpoints

### Anthropic-compatible

#### POST /v1/messages

Accepts Anthropic Messages API format. Automatically converts to the upstream provider's format if needed.

#### GET /v1/models (Anthropic shape)

Returns the model list in Anthropic format when the request uses `x-api-key` or no authentication header.

### OpenAI-compatible

#### POST /v1/chat/completions

Accepts OpenAI Chat Completions format. Normalised to Anthropic canonical form internally; response converted back on the way out.

#### POST /v1/responses

Accepts OpenAI Responses API format. Same normalisation pipeline as `/v1/chat/completions`.

#### GET /v1/models (OpenAI shape)

Returns the model list in OpenAI format when the request uses a `Bearer` token.

### Utility

#### GET /health

```json
{
  "status": "ok",
  "provider": "anthropic-official",
  "version": "0.1.0"
}
```

#### POST /reload

Reload configuration from disk without restarting:

```bash
xh POST http://localhost:7896/reload
```

## Format Conversion

### Anthropic → OpenAI

- `system` → system message
- `messages` → messages array
- `tool_use` → `tool_calls`
- `tool_result` → tool role message
- `thinking` blocks → `reasoning_content`
- `stop_sequences` → `stop`

### OpenAI → Anthropic

- `tool_calls` → `tool_use` blocks
- `reasoning_content` → `thinking` blocks
- `finish_reason` mapping:
  - `stop` → `end_turn`
  - `length` → `max_tokens`
  - `tool_calls` → `tool_use`

## Security Notes

- API keys starting with `$` are resolved from environment variables
- Configuration file should have restricted permissions (0600)
- API keys are masked in TUI when not focused
- Error messages do not expose sensitive information

## Development

### Build

```bash
cargo build --release
```

### Run tests

```bash
cargo test
```

### Run clippy

```bash
cargo clippy --all-targets --all-features
```

## License

See LICENSE file for details.
