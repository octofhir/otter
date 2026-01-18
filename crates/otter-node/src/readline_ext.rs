//! Readline extension module.
//!
//! Provides the node:readline module for reading input line by line.
//! Used for CLI applications and interactive prompts.

use otter_runtime::Extension;

/// Create the readline extension.
///
/// This extension provides the Node.js-compatible readline interface:
/// - `readline.createInterface(options)` - create a new Interface
/// - `Interface.question(query, callback)` - prompt for input
/// - `Interface.close()` - close the interface
/// - Events: 'line', 'close', 'pause', 'resume'
///
/// The implementation is JavaScript-based, building on EventEmitter.
pub fn extension() -> Extension {
    Extension::new("readline").with_js(include_str!("readline.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "readline");
        assert!(ext.js_code().is_some());
    }
}
