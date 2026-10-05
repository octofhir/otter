//! The CommonJS module wrapper: the one compile unit a CommonJS body becomes.
//!
//! # Contents
//! - [`wrapper_source`] formats
//!   `(function (exports, require, module, __filename, __dirname) { <body>\n})`.
//!
//! # Invariants
//! - The whole prologue sits on line 1, so body line `N` is wrapped line `N`;
//!   the module's source registers the prologue length, so line-1 columns are
//!   the file's own too, as Node's `compileFunction` reports them.
//! - File modules wrapped at run time and builtin modules wrapped at product
//!   build time use this one text, so positions and
//!   `Function.prototype.toString` agree between them.
//!
//! # See also
//! - `otter_vm::Interpreter::create_commonjs_wrapper` runs the wrapper.

/// The text before a CommonJS body.
pub const WRAPPER_PREFIX: &str = "(function (exports, require, module, __filename, __dirname) { ";

/// The text after a CommonJS body.
pub const WRAPPER_SUFFIX: &str = "\n})";

/// The source of the function a CommonJS module body runs in.
#[must_use]
pub fn wrapper_source(body: &str) -> String {
    let mut source =
        String::with_capacity(WRAPPER_PREFIX.len() + body.len() + WRAPPER_SUFFIX.len());
    source.push_str(WRAPPER_PREFIX);
    source.push_str(body);
    source.push_str(WRAPPER_SUFFIX);
    source
}

#[cfg(test)]
mod tests {
    #[test]
    fn wrapper_keeps_the_prologue_on_the_first_line() {
        assert_eq!(
            super::wrapper_source("a;\nb;"),
            "(function (exports, require, module, __filename, __dirname) { a;\nb;\n})"
        );
    }
}
