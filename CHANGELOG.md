# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Fixed

#### Critical
- **Security**: API key error messages no longer expose sensitive information
- **Concurrency**: Removed hot-reload on every request to prevent race conditions and excessive I/O
  - Configuration now reloaded via explicit `/reload` endpoint or `r` key in TUI
  - Improved request throughput by using read locks instead of write locks
- **Streaming**: Stream errors now send proper error events to clients instead of silently breaking

#### Major
- **Performance**: HTTP client now reused across requests via shared state (connection pooling)
- **Code Quality**: Refactored URL construction logic to eliminate duplication
- **Maintainability**: Extracted hardcoded model names to constants
- **Data Integrity**: Changed `swap_remove` to `shift_remove` to preserve provider order in UI

#### Minor
- **Code Quality**: Removed unnecessary `#[allow(dead_code)]` attributes
- **Maintainability**: Extracted magic numbers to named constants (`MESSAGE_TIMEOUT_SECS`, `HIGHLIGHT_BG_INDEX`, `TEST_TIMEOUT_SECS`)
- **API**: Enhanced `/health` endpoint to include version information

### Added
- **Feature**: `r` keybinding in TUI to reload configuration from disk
- **Feature**: TUI now auto-starts proxy server on launch if a provider is configured
- **API**: New `/reload` endpoint for hot-reloading configuration
- **Documentation**: Comprehensive README.md with usage examples and API documentation

### Changed
- **Architecture**: Refactored shared state from `SharedConfig` to `SharedState` containing both config and HTTP client
- **Error Handling**: Improved error messages to avoid leaking sensitive information

## [0.1.0] - 2026-03-17

### Added
- Initial implementation of Claude Code Switch proxy
- Anthropic ↔ OpenAI API format bidirectional transformation
- Multi-provider configuration with hot-switching
- TUI with provider management (add/edit/delete/test)
- Embedded proxy server with graceful shutdown
- Model mapping and tool/thinking parameter translation
- Full streaming support for both API formats
- Comprehensive test coverage for transformation logic
