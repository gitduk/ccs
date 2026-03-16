# CCS - Claude Code Switch

A lightweight API proxy for routing Claude Code traffic between multiple providers with Anthropic ↔ OpenAI format conversion.

## Features

- **Multi-Provider Support**: Configure and switch between multiple API providers
- **Format Conversion**: Automatic bidirectional translation between Anthropic and OpenAI API formats
- **Streaming Support**: Full support for Server-Sent Events (SSE) streaming responses
- **TUI Management**: Interactive terminal UI for provider configuration
- **Hot Reload**: Reload configuration without restarting the proxy (press `r` in TUI or POST to `/reload`)
- **Model Mapping**: Custom model name mapping per provider
- **Tool Calling**: Full support for function/tool calling in both formats
- **Extended Thinking**: Support for Claude's thinking/reasoning blocks

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

Set the environment variable to use the proxy:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:7896
```

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
      "model_map": {}
    },
    "openrouter": {
      "base_url": "https://openrouter.ai/api",
      "api_key": "$OPENROUTER_API_KEY",
      "api_format": "openai",
      "model_map": {
        "claude-sonnet-4-20250514": "anthropic/claude-sonnet-4-20250514"
      }
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

## TUI Keybindings

- `↑/↓` or `j/k` - Navigate providers
- `s` - Switch to selected provider
- `a` - Add new provider
- `e` - Edit selected provider
- `d` - Delete selected provider
- `t` - Test connectivity
- `p` - Start/stop proxy server
- `r` - Reload configuration from disk
- `q` or `Esc` - Quit

## API Endpoints

### POST /v1/messages

Main proxy endpoint. Accepts Anthropic Messages API format and forwards to the current provider with automatic format conversion if needed.

### GET /health

Health check endpoint:

```json
{
  "status": "ok",
  "provider": "anthropic-official",
  "version": "0.1.0"
}
```

### POST /reload

Reload configuration from disk without restarting:

```bash
curl -X POST http://localhost:7896/reload
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
