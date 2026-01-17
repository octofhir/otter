# Changelog

All notable changes to this project will be documented in this file.

## [0.1.0] - 2026-01-17

### Added

- Initial release
- `#[dive]` attribute macro for marking Rust functions callable from JavaScript
- `#[dive(swift)]` mode for synchronous functions
- `#[dive(deep)]` mode for async functions returning Promises
- Automatic argument deserialization from JSON
- Automatic result serialization to JSON
- Support for `Result<T, E>` return types with error mapping
