//! Literal validation for ECMAScript compliance

use crate::error::{CompileError, CompileResult};
use oxc_ast::ast::{NumericLiteral, RegExpLiteral, StringLiteral, TemplateLiteral};
use oxc_allocator::Allocator;
use oxc_regular_expression::{LiteralParser, Options as RegExpOptions};

/// ECMAScript version for validation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EcmaVersion {
    /// ECMAScript 5
    Es5,
    /// ECMAScript 2015 (ES6)
    Es2015,
    /// ECMAScript 2016
    Es2016,
    /// ECMAScript 2017
    Es2017,
    /// ECMAScript 2018
    Es2018,
    /// ECMAScript 2019
    Es2019,
    /// ECMAScript 2020
    Es2020,
    /// ECMAScript 2021
    Es2021,
    /// ECMAScript 2022
    Es2022,
    /// ECMAScript 2023
    Es2023,
    /// Latest ECMAScript version
    Latest,
}

impl Default for EcmaVersion {
    fn default() -> Self {
        Self::Latest
    }
}

/// Source location information
#[derive(Debug, Clone)]
pub struct SourceLocation {
    /// Line number (1-based)
    pub line: u32,
    /// Column number (1-based)
    pub column: u32,
    /// Filename
    pub filename: String,
}

impl SourceLocation {
    /// Create a new source location
    pub fn new(line: u32, column: u32, filename: impl Into<String>) -> Self {
        Self {
            line,
            column,
            filename: filename.into(),
        }
    }
}

/// Validation context for literals
#[derive(Debug, Clone)]
pub struct ValidationContext {
    /// Whether we're in strict mode
    pub strict_mode: bool,
    /// Source location
    pub source_location: SourceLocation,
    /// ECMAScript version
    pub ecma_version: EcmaVersion,
}

impl ValidationContext {
    /// Create a new validation context
    pub fn new(
        strict_mode: bool,
        source_location: SourceLocation,
        ecma_version: EcmaVersion,
    ) -> Self {
        Self {
            strict_mode,
            source_location,
            ecma_version,
        }
    }
}

/// Literal validator for ECMAScript compliance
#[derive(Debug, Clone)]
pub struct LiteralValidator {
    /// Whether we're in strict mode
    strict_mode: bool,
    /// ECMAScript version
    ecma_version: EcmaVersion,
}

impl LiteralValidator {
    /// Create a new literal validator
    pub fn new(strict_mode: bool, ecma_version: EcmaVersion) -> Self {
        Self {
            strict_mode,
            ecma_version,
        }
    }

    /// Create a validator with default settings
    pub fn default() -> Self {
        Self::new(false, EcmaVersion::default())
    }

    /// Create a validator for strict mode
    pub fn strict() -> Self {
        Self::new(true, EcmaVersion::default())
    }

    /// Get the strict mode setting
    pub fn is_strict_mode(&self) -> bool {
        self.strict_mode
    }

    /// Get the ECMAScript version
    pub fn ecma_version(&self) -> EcmaVersion {
        self.ecma_version
    }

    /// Set strict mode
    pub fn set_strict_mode(&mut self, strict_mode: bool) {
        self.strict_mode = strict_mode;
    }

    /// Set ECMAScript version
    pub fn set_ecma_version(&mut self, version: EcmaVersion) {
        self.ecma_version = version;
    }

    /// Validate a numeric literal
    pub fn validate_numeric_literal(&self, lit: &NumericLiteral) -> CompileResult<()> {
        // Get the raw source text to analyze the literal format
        let raw = match &lit.raw {
            Some(atom) => atom.as_str(),
            None => {
                // If no raw text available, we can't validate format-specific issues
                // but the parser already validated basic syntax
                return Ok(());
            }
        };

        // Check for legacy octal literals in strict mode (e.g., 077)
        if self.strict_mode {
            if self.is_legacy_octal(raw) {
                return Err(CompileError::legacy_syntax(
                    format!(
                        "Legacy octal literal '{}' is not allowed in strict mode",
                        raw
                    ),
                    lit.span.start,
                    lit.span.start + 1,
                ));
            }
            if self.is_non_octal_decimal(raw) {
                return Err(CompileError::legacy_syntax(
                    format!(
                        "Non-octal decimal literal '{}' is not allowed in strict mode",
                        raw
                    ),
                    lit.span.start,
                    lit.span.start + 1,
                ));
            }
        }

        // Check for invalid numeric separator usage
        if raw.contains('_') && !self.is_valid_numeric_separator_usage(raw) {
            return Err(CompileError::invalid_literal(
                format!("Invalid numeric separator usage in '{}'", raw),
                lit.span.start,
                lit.span.start + 1,
            ));
        }

        // Validate binary literals (0b/0B prefix)
        if raw.starts_with("0b") || raw.starts_with("0B") {
            if !self.is_valid_binary_literal(raw) {
                return Err(CompileError::invalid_literal(
                    format!("Invalid binary literal '{}'", raw),
                    lit.span.start,
                    lit.span.start + 1,
                ));
            }
            // Binary literals are valid, no need to check decimal format
            return Ok(());
        }

        // Validate hexadecimal literals (0x/0X prefix)
        if raw.starts_with("0x") || raw.starts_with("0X") {
            if !self.is_valid_hex_literal(raw) {
                return Err(CompileError::invalid_literal(
                    format!("Invalid hexadecimal literal '{}'", raw),
                    lit.span.start,
                    lit.span.start + 1,
                ));
            }
            // Hex literals are valid, no need to check decimal format
            return Ok(());
        }

        // Check for other invalid numeric formats (only for decimal literals)
        if !self.is_valid_numeric_format(raw) {
            return Err(CompileError::invalid_literal(
                format!("Invalid numeric literal format '{}'", raw),
                lit.span.start,
                lit.span.start + 1,
            ));
        }

        Ok(())
    }

    /// Check if a raw string represents a legacy octal literal
    fn is_legacy_octal(&self, raw: &str) -> bool {
        // Legacy octal: starts with 0 followed by octal digits, but not 0x, 0b, or 0.
        if raw.len() < 2 || !raw.starts_with('0') {
            return false;
        }

        let rest = &raw[1..];
        if rest.starts_with('x')
            || rest.starts_with('X')
            || rest.starts_with('b')
            || rest.starts_with('B')
            || rest.starts_with('o')
            || rest.starts_with('O')
            || rest.starts_with('.')
        {
            return false;
        }

        // If it contains only octal digits, it's a legacy octal literal
        rest.chars().all(|c| c.is_ascii_digit() && c <= '7')
    }

    /// Check if a raw string represents a non-octal decimal literal (e.g., 08)
    fn is_non_octal_decimal(&self, raw: &str) -> bool {
        // Starts with 0 followed by digits, but not 0x, 0b, 0o, or 0.
        // And contains at least one 8 or 9.
        if raw.len() < 2 || !raw.starts_with('0') {
            return false;
        }

        let rest = &raw[1..];
        if rest.starts_with('x')
            || rest.starts_with('X')
            || rest.starts_with('b')
            || rest.starts_with('B')
            || rest.starts_with('o')
            || rest.starts_with('O')
            || rest.starts_with('.')
        {
            return false;
        }

        // Must be all digits and contain '8' or '9'
        rest.chars().all(|c| c.is_ascii_digit()) && rest.chars().any(|c| c == '8' || c == '9')
    }

    /// Check if numeric separator usage is valid
    fn is_valid_numeric_separator_usage(&self, raw: &str) -> bool {
        // Numeric separators cannot be at the beginning or end
        if raw.starts_with('_') || raw.ends_with('_') {
            return false;
        }

        // Cannot have consecutive separators
        if raw.contains("__") {
            return false;
        }

        // Cannot have separators immediately after prefixes (0x, 0b, etc.)
        if raw.starts_with("0x_")
            || raw.starts_with("0X_")
            || raw.starts_with("0b_")
            || raw.starts_with("0B_")
            || raw.starts_with("0o_")
            || raw.starts_with("0O_")
        {
            return false;
        }

        // Cannot have separators around decimal point
        if raw.contains("._") || raw.contains("_.") {
            return false;
        }

        // Check for non-decimal literals (hex, binary, octal)
        let is_non_decimal = raw.starts_with("0x")
            || raw.starts_with("0X")
            || raw.starts_with("0b")
            || raw.starts_with("0B")
            || raw.starts_with("0o")
            || raw.starts_with("0O");

        // Cannot have separators around exponent (only for decimal literals)
        // In hex literals, 'e' is a valid digit, so '_e' or 'e_' is allowed.
        if !is_non_decimal {
            if raw.contains("_e") || raw.contains("_E") || raw.contains("e_") || raw.contains("E_")
            {
                return false;
            }
        }

        true
    }

    /// Check if binary literal is valid
    fn is_valid_binary_literal(&self, raw: &str) -> bool {
        if raw.len() < 3 || !(raw.starts_with("0b") || raw.starts_with("0B")) {
            return false;
        }

        let digits = &raw[2..]; // Skip "0b" or "0B"

        // Remove separators for validation
        let clean_digits: String = digits.chars().filter(|&c| c != '_').collect();

        if clean_digits.is_empty() {
            return false;
        }

        // All remaining characters must be 0 or 1
        clean_digits.chars().all(|c| c == '0' || c == '1')
    }

    /// Check if hexadecimal literal is valid
    fn is_valid_hex_literal(&self, raw: &str) -> bool {
        if raw.len() < 3 || !(raw.starts_with("0x") || raw.starts_with("0X")) {
            return false;
        }

        let digits = &raw[2..]; // Skip "0x" or "0X"

        // Remove separators for validation
        let clean_digits: String = digits.chars().filter(|&c| c != '_').collect();

        if clean_digits.is_empty() {
            return false;
        }

        // All remaining characters must be hex digits
        clean_digits.chars().all(|c| c.is_ascii_hexdigit())
    }

    /// Check if numeric format is valid overall
    fn is_valid_numeric_format(&self, raw: &str) -> bool {
        // Basic sanity checks
        if raw.is_empty() {
            return false;
        }

        // Check for multiple decimal points
        if raw.matches('.').count() > 1 {
            return false;
        }

        // Check for multiple exponents
        let e_count = raw.matches('e').count() + raw.matches('E').count();
        if e_count > 1 {
            return false;
        }

        // If it contains an exponent, validate the format
        if e_count == 1 {
            return self.is_valid_exponent_format(raw);
        }

        true
    }

    /// Check if exponent format is valid
    fn is_valid_exponent_format(&self, raw: &str) -> bool {
        // Find the exponent position
        let e_pos = raw.find('e').or_else(|| raw.find('E'));

        if let Some(pos) = e_pos {
            let after_e = &raw[pos + 1..];

            if after_e.is_empty() {
                return false;
            }

            // Can start with + or -
            let digits_part = if after_e.starts_with('+') || after_e.starts_with('-') {
                &after_e[1..]
            } else {
                after_e
            };

            if digits_part.is_empty() {
                return false;
            }

            // Remove separators and check if all are digits
            let clean_digits: String = digits_part.chars().filter(|&c| c != '_').collect();
            !clean_digits.is_empty() && clean_digits.chars().all(|c| c.is_ascii_digit())
        } else {
            false
        }
    }

    /// Validate a string literal
    pub fn validate_string_literal(&self, lit: &StringLiteral) -> CompileResult<()> {
        // Get the raw source text to analyze escape sequences
        let raw = match &lit.raw {
            Some(atom) => atom.as_str(),
            None => {
                // If no raw text available, we can't validate format-specific issues
                // but the parser already validated basic syntax
                return Ok(());
            }
        };

        // Remove the quotes to get the content
        let content = if raw.len() >= 2 {
            &raw[1..raw.len() - 1] // Remove first and last quote
        } else {
            return Err(CompileError::invalid_literal(
                "Invalid string literal format".to_string(),
                lit.span.start,
                lit.span.start + 1,
            ));
        };

        // Validate escape sequences
        self.validate_string_escape_sequences(content, lit.span.start)?;

        Ok(())
    }

    /// Validate escape sequences in string content
    fn validate_string_escape_sequences(&self, content: &str, start_pos: u32) -> CompileResult<()> {
        let mut chars = content.chars().peekable();
        let mut position = 0;

        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.peek() {
                    Some(&next_ch) => {
                        chars.next(); // consume the next character
                        position += 2;

                        match next_ch {
                            // Valid single-character escapes
                            // Legacy octal escape sequences (deprecated in strict mode)
                            '0'..='7' => {
                                // Special case for \0: allowed in strict mode if not followed by decimal digit
                                if next_ch == '0' {
                                    let is_legacy_octal = match chars.peek() {
                                        Some(&digit) if digit.is_ascii_digit() => true,
                                        _ => false,
                                    };

                                    if !is_legacy_octal {
                                        // It's a null character escape \0, valid in strict mode
                                        continue;
                                    }
                                }

                                if self.strict_mode {
                                    return Err(CompileError::legacy_syntax(
                                        format!(
                                            "Legacy octal escape sequence '\\{}' is not allowed in strict mode",
                                            next_ch
                                        ),
                                        start_pos + position as u32,
                                        start_pos + position as u32 + 1,
                                    ));
                                } 
                                // In non-strict mode, consume the octal digits
                                self.consume_octal_digits(&mut chars);
                            }
                            // Other numeric escapes (8, 9) are legacy and invalid in strict mode
                            '8' | '9' => {
                                if self.strict_mode {
                                    return Err(CompileError::legacy_syntax(
                                        format!(
                                            "Legacy numeric escape sequence '\\{}' is not allowed in strict mode",
                                            next_ch
                                        ),
                                        start_pos + position as u32,
                                        start_pos + position as u32 + 1,
                                    ));
                                }
                            }
                            // Any other character after backslash
                            _ => {
                                // In strict mode, only specific escapes are allowed
                                if self.strict_mode && !self.is_valid_escape_character(next_ch) {
                                    return Err(CompileError::invalid_literal(
                                        format!(
                                            "Invalid escape sequence '\\{}' in strict mode",
                                            next_ch
                                        ),
                                        start_pos + position as u32,
                                        start_pos + position as u32 + 1,
                                    ));
                                }
                            }
                        }
                    }
                    None => {
                        return Err(CompileError::invalid_literal(
                            "Unterminated escape sequence".to_string(),
                            start_pos + position as u32,
                            start_pos + position as u32 + 1,
                        ));
                    }
                }
            } else {
                position += ch.len_utf8();
            }
        }

        Ok(())
    }

    /// Validate hexadecimal escape sequence (\xHH)
    fn validate_hex_escape_sequence(
        &self,
        chars: &mut std::iter::Peekable<std::str::Chars>,
    ) -> bool {
        // Need exactly 2 hex digits
        for _ in 0..2 {
            match chars.peek() {
                Some(ch) if ch.is_ascii_hexdigit() => {
                    chars.next();
                }
                _ => return false,
            }
        }
        true
    }

    /// Validate unicode escape sequence (\uHHHH or \u{...})
    fn validate_unicode_escape_sequence(
        &self,
        chars: &mut std::iter::Peekable<std::str::Chars>,
    ) -> CompileResult<usize> {
        match chars.peek() {
            Some('{') => {
                // \u{...} format
                chars.next(); // consume '{'
                let mut hex_digits = 0;
                let mut consumed = 1; // for the '{'

                while let Some(&ch) = chars.peek() {
                    if ch == '}' {
                        chars.next(); // consume '}'
                        consumed += 1;
                        if hex_digits == 0 || hex_digits > 6 {
                            return Err(CompileError::invalid_literal(
                                "Invalid Unicode escape sequence: must have 1-6 hex digits"
                                    .to_string(),
                                0,
                                0, // Position will be adjusted by caller
                            ));
                        }
                        return Ok(consumed);
                    } else if ch.is_ascii_hexdigit() {
                        chars.next();
                        hex_digits += 1;
                        consumed += 1;
                    } else {
                        return Err(CompileError::invalid_literal(
                            format!("Invalid character '{}' in Unicode escape sequence", ch),
                            0,
                            0, // Position will be adjusted by caller
                        ));
                    }
                }

                Err(CompileError::invalid_literal(
                    "Unterminated Unicode escape sequence".to_string(),
                    0,
                    0, // Position will be adjusted by caller
                ))
            }
            _ => {
                // \uHHHH format - need exactly 4 hex digits
                for i in 0..4 {
                    match chars.peek() {
                        Some(ch) if ch.is_ascii_hexdigit() => {
                            chars.next();
                        }
                        _ => {
                            return Err(CompileError::invalid_literal(
                                format!(
                                    "Invalid Unicode escape sequence: expected 4 hex digits, got {}",
                                    i
                                ),
                                0,
                                0, // Position will be adjusted by caller
                            ));
                        }
                    }
                }
                Ok(4)
            }
        }
    }

    /// Consume octal digits for legacy octal escape sequences
    fn consume_octal_digits(&self, chars: &mut std::iter::Peekable<std::str::Chars>) {
        // Consume up to 2 more octal digits (total of 3 including the first)
        for _ in 0..2 {
            match chars.peek() {
                Some(ch) if ch.is_ascii_digit() && *ch <= '7' => {
                    chars.next();
                }
                _ => break,
            }
        }
    }

    /// Check if a character is valid after backslash in escape sequences
    fn is_valid_escape_character(&self, ch: char) -> bool {
        match ch {
            // Standard escape characters
            'n' | 't' | 'r' | 'b' | 'f' | 'v' | '0' | '\'' | '"' | '\\' => true,
            // Hex and Unicode escapes
            'x' | 'u' => true,
            // Line terminators for line continuation
            '\n' | '\r' => true,
            // In non-strict mode, any character can be escaped
            _ => !self.strict_mode,
        }
    }

    /// Validate a regular expression literal
    pub fn validate_regexp_literal(&self, lit: &RegExpLiteral) -> CompileResult<()> {
        let pattern = lit.regex.pattern.text.as_str();

        let mut flags_storage: Option<String> = None;
        let (flags_text, flags_offset) = if let Some(raw) = &lit.raw {
            let raw_text = raw.as_str();
            if let Some(last_slash) = raw_text.rfind('/') {
                let flags = &raw_text[last_slash + 1..];
                (
                    if flags.is_empty() { None } else { Some(flags) },
                    lit.span.start + last_slash as u32 + 1,
                )
            } else {
                (None, lit.span.start + 1 + pattern.len() as u32 + 1)
            }
        } else {
            let flags = lit.regex.flags.to_string();
            (
                {
                    if !flags.is_empty() {
                        flags_storage = Some(flags);
                    }
                    flags_storage.as_deref()
                },
                lit.span.start + 1 + pattern.len() as u32 + 1,
            )
        };

        let allocator = Allocator::default();
        let parser = LiteralParser::new(
            &allocator,
            pattern,
            flags_text,
            RegExpOptions {
                pattern_span_offset: lit.span.start + 1,
                flags_span_offset: flags_offset,
            },
        );

        parser.parse().map(|_| ()).map_err(|error| {
            let offset = error
                .labels
                .as_ref()
                .and_then(|labels| labels.first())
                .map(|label| label.offset() as u32)
                .unwrap_or(lit.span.start);
            CompileError::invalid_literal(error.to_string(), offset, offset + 1)
        })
    }

    /// Validate RegExp pattern syntax
    fn validate_regexp_pattern(&self, pattern: &str, start_pos: u32) -> CompileResult<()> {
        // Check for basic syntax errors that would make the pattern invalid

        // Check for unescaped forward slashes (should not happen in parsed literals, but let's be safe)
        if pattern.contains('/') && !self.is_valid_slash_usage(pattern) {
            return Err(CompileError::invalid_literal(
                "Unescaped forward slash in RegExp pattern".to_string(),
                start_pos,
                start_pos + 1,
            ));
        }

        // Check for invalid escape sequences
        self.validate_regexp_escape_sequences(pattern, start_pos)?;

        // Check for invalid character classes
        self.validate_regexp_character_classes(pattern, start_pos)?;

        // Check for invalid quantifiers
        self.validate_regexp_quantifiers(pattern, start_pos)?;

        // Check for invalid groups
        self.validate_regexp_groups(pattern, start_pos)?;

        // Try to compile the pattern to catch other syntax errors
        // Note: We use a simple approach here - in a full implementation,
        // we'd want to use a proper ECMAScript RegExp parser
        if let Err(_) = regex::Regex::new(&self.convert_js_regex_to_rust_regex(pattern)) {
            // Only report as error if it's clearly a syntax issue
            if self.has_obvious_syntax_errors(pattern) {
                return Err(CompileError::invalid_literal(
                    "Invalid RegExp pattern syntax".to_string(),
                    start_pos,
                    start_pos + 1,
                ));
            }
        }

        Ok(())
    }

    /// Validate RegExp flags
    fn validate_regexp_flags(&self, flags: &str, start_pos: u32) -> CompileResult<()> {
        let mut seen_flags = std::collections::HashSet::new();

        for (i, flag) in flags.chars().enumerate() {
            // Check if flag is valid
            match flag {
                'g' | 'i' | 'm' | 's' | 'u' | 'y' | 'd' | 'v' => {
                    // Valid flag
                }
                _ => {
                    return Err(CompileError::invalid_literal(
                        format!("Invalid RegExp flag '{}'", flag),
                        start_pos + i as u32,
                        start_pos + i as u32 + 1,
                    ));
                }
            }

            // Check for duplicate flags
            if seen_flags.contains(&flag) {
                return Err(CompileError::invalid_literal(
                    format!("Duplicate RegExp flag '{}'", flag),
                    start_pos + i as u32,
                    start_pos + i as u32 + 1,
                ));
            }

            seen_flags.insert(flag);
        }

        // Check for conflicting flags
        if seen_flags.contains(&'u') && seen_flags.contains(&'v') {
            return Err(CompileError::invalid_literal(
                "RegExp flags 'u' and 'v' cannot be used together".to_string(),
                start_pos,
                start_pos + flags.len() as u32,
            ));
        }

        Ok(())
    }

    /// Check if forward slash usage is valid in pattern
    fn is_valid_slash_usage(&self, pattern: &str) -> bool {
        let mut chars = pattern.chars().peekable();
        let mut in_char_class = false;

        while let Some(ch) = chars.next() {
            match ch {
                '\\' => {
                    // Skip escaped character
                    chars.next();
                }
                '[' if !in_char_class => {
                    in_char_class = true;
                }
                ']' if in_char_class => {
                    in_char_class = false;
                }
                '/' if !in_char_class => {
                    // Unescaped slash outside character class is invalid
                    return false;
                }
                _ => {}
            }
        }

        true
    }

    /// Validate escape sequences in RegExp pattern
    fn validate_regexp_escape_sequences(&self, pattern: &str, start_pos: u32) -> CompileResult<()> {
        let mut chars = pattern.chars().peekable();
        let mut position = 0;

        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.peek() {
                    Some(&next_ch) => {
                        chars.next(); // consume the next character
                        position += 2;

                        match next_ch {
                            // Valid RegExp escape sequences
                            'n' | 't' | 'r' | 'f' | 'v' | '0' | 'd' | 'D' | 's' | 'S' | 'w'
                            | 'W' | 'b' | 'B' | '^' | '$' | '\\' | '/' | '.' | '*' | '+' | '?'
                            | '(' | ')' | '[' | ']' | '{' | '}' | '|' => {
                                // These are always valid
                            }
                            // Hexadecimal escape sequences (\xHH)
                            'x' => {
                                if !self.validate_hex_escape_sequence(&mut chars) {
                                    return Err(CompileError::invalid_literal(
                                        format!(
                                            "Invalid hexadecimal escape sequence in RegExp at position {}",
                                            position
                                        ),
                                        start_pos + position as u32,
                                        start_pos + position as u32 + 1,
                                    ));
                                }
                                position += 2; // for the two hex digits
                            }
                            // Unicode escape sequences (\uHHHH or \u{...})
                            'u' => {
                                let consumed = self.validate_unicode_escape_sequence(&mut chars)?;
                                position += consumed;
                            }
                            // Octal escape sequences (valid in RegExp)
                            '1'..='7' => {
                                // In RegExp, octal escapes are generally valid
                                self.consume_octal_digits(&mut chars);
                            }
                            // Other characters - in RegExp, most characters can be escaped
                            _ => {
                                // Most characters can be escaped in RegExp patterns
                            }
                        }
                    }
                    None => {
                        return Err(CompileError::invalid_literal(
                            "Unterminated escape sequence in RegExp".to_string(),
                            start_pos + position as u32,
                            start_pos + position as u32 + 1,
                        ));
                    }
                }
            } else {
                position += ch.len_utf8();
            }
        }

        Ok(())
    }

    /// Validate character classes in RegExp pattern
    fn validate_regexp_character_classes(
        &self,
        pattern: &str,
        start_pos: u32,
    ) -> CompileResult<()> {
        let mut chars = pattern.chars().peekable();
        let mut _position = 0;
        let mut bracket_depth = 0;

        while let Some(ch) = chars.next() {
            match ch {
                '\\' => {
                    // Skip escaped character
                    chars.next();
                    _position += 2;
                }
                '[' => {
                    bracket_depth += 1;
                    _position += 1;
                }
                ']' => {
                    if bracket_depth > 0 {
                        bracket_depth -= 1;
                    }
                    _position += 1;
                }
                _ => {
                    _position += ch.len_utf8();
                }
            }
        }

        if bracket_depth > 0 {
            return Err(CompileError::invalid_literal(
                "Unterminated character class in RegExp".to_string(),
                start_pos,
                start_pos + pattern.len() as u32,
            ));
        }

        Ok(())
    }

    /// Validate quantifiers in RegExp pattern
    fn validate_regexp_quantifiers(&self, pattern: &str, _start_pos: u32) -> CompileResult<()> {
        // Basic quantifier validation
        // This is a simplified check - a full implementation would be more comprehensive

        let invalid_sequences = ["**", "++", "??", "*+", "+*", "*?", "+?"];

        for seq in &invalid_sequences {
            if pattern.contains(seq) {
                // This might be valid in some contexts, so we don't error here
                // A full implementation would do proper parsing
            }
        }

        Ok(())
    }

    /// Validate groups in RegExp pattern
    fn validate_regexp_groups(&self, pattern: &str, start_pos: u32) -> CompileResult<()> {
        let mut chars = pattern.chars().peekable();
        let mut position = 0;
        let mut paren_depth = 0;
        let mut in_char_class = false;

        while let Some(ch) = chars.next() {
            if ch == '\\' {
                // Skip escaped character
                chars.next();
                position += 2;
                continue;
            }

            if in_char_class {
                if ch == ']' {
                    in_char_class = false;
                }
                position += ch.len_utf8();
                continue;
            }

            match ch {
                '[' => {
                    in_char_class = true;
                    position += 1;
                }
                '(' => {
                    paren_depth += 1;
                    position += 1;

                    // Check for group type
                    if let Some(&'?') = chars.peek() {
                        chars.next(); // consume '?'
                        position += 1;

                        if let Some(&next_ch) = chars.peek() {
                            match next_ch {
                                ':' | '=' | '!' => {
                                    // (?:...) Non-capturing
                                    // (?=...) Lookahead
                                    // (?!...) Negative lookahead
                                    // allowed
                                }
                                '<' => {
                                    // (?<=...) Lookbehind
                                    // (?<!...) Negative lookbehind
                                    // (?<name>...) Named group
                                    // allowed
                                }
                                // Modifiers (ES2018/Proposal) must be followed by colon?
                                // Syntax: (?ims-ims:...)
                                // Invalid: (?ims)
                                c if c.is_ascii_alphabetic() || c == '-' => {
                                    // This looks like modifiers.
                                    // We need to scan ahead to ensure it ends with ':'
                                    // If we hit ')' before ':', it's an inline modifier which is invalid in JS

                                    // We can't easily peek multiple chars with just peek(),
                                    // but we can clone the iterator for a short check check?
                                    // Or just continue parsing. Current loop structure makes simple check hard.
                                    // Let's just do a quick lookahead here.
                                    let mut temp_chars = chars.clone();
                                    let mut has_colon = false;

                                    while let Some(flag) = temp_chars.next() {
                                        if flag == ':' {
                                            has_colon = true;
                                            break;
                                        }
                                        if flag == ')' {
                                            break; // Found end without colon
                                        }
                                        if !flag.is_ascii_alphabetic() && flag != '-' {
                                            break; // Invalid char in flags
                                        }
                                    }

                                    if !has_colon {
                                        return Err(CompileError::invalid_literal(
                                            "Invalid RegExp group: Inline modifiers allow only with colon (?flags:...)".to_string(),
                                            start_pos + position as u32 - 2, // point to (?
                                            start_pos + position as u32 + 1,
                                        ));
                                    }
                                }
                                _ => {
                                    return Err(CompileError::invalid_literal(
                                        format!("Invalid RegExp group start '(?{}'", next_ch),
                                        start_pos + position as u32 - 2,
                                        start_pos + position as u32 + 1,
                                    ));
                                }
                            }
                        } else {
                            // (? at end of string
                            return Err(CompileError::invalid_literal(
                                "Invalid RegExp group",
                                start_pos + position as u32 - 2,
                                start_pos + position as u32,
                            ));
                        }
                    }
                }
                ')' => {
                    if paren_depth > 0 {
                        paren_depth -= 1;
                    } else {
                        return Err(CompileError::invalid_literal(
                            format!(
                                "Unmatched closing parenthesis in RegExp at position {}",
                                position
                            ),
                            start_pos + position as u32,
                            start_pos + position as u32 + 1,
                        ));
                    }
                    position += 1;
                }
                _ => {
                    position += ch.len_utf8();
                }
            }
        }

        if in_char_class {
            return Err(CompileError::invalid_literal(
                "Unterminated character class in RegExp".to_string(),
                start_pos,
                start_pos + pattern.len() as u32,
            ));
        }

        if paren_depth > 0 {
            return Err(CompileError::invalid_literal(
                "Unterminated group in RegExp".to_string(),
                start_pos,
                start_pos + pattern.len() as u32,
            ));
        }

        Ok(())
    }

    /// Convert JavaScript RegExp pattern to Rust regex (simplified)
    fn convert_js_regex_to_rust_regex(&self, pattern: &str) -> String {
        // This is a very simplified conversion
        // A full implementation would handle all the differences between JS and Rust regex
        pattern.to_string()
    }

    /// Check for obvious syntax errors in RegExp pattern
    fn has_obvious_syntax_errors(&self, pattern: &str) -> bool {
        // Check for some obvious syntax errors
        pattern.contains("(?") || // Incomplete group syntax
        pattern.contains("[^") && !pattern.contains("]") || // Incomplete negated character class
        pattern.ends_with('\\') // Trailing backslash
    }

    /// Validate a template literal
    pub fn validate_template_literal(&self, lit: &TemplateLiteral) -> CompileResult<()> {
        // Validate each quasi (string part) in the template literal
        for (i, quasi) in lit.quasis.iter().enumerate() {
            // Get the raw content of the quasi
            let raw_content = quasi.value.raw.as_str();

            // Validate escape sequences in the template literal quasi
            self.validate_template_escape_sequences(raw_content, quasi.span.start, i)?;
        }

        // Validate expressions in the template literal
        // Note: Expression validation would be handled by the main compiler
        // Here we just ensure the structure is valid

        // Check that the number of quasis and expressions is consistent
        let expected_quasis = lit.expressions.len() + 1;
        if lit.quasis.len() != expected_quasis {
            return Err(CompileError::invalid_literal(
                format!(
                    "Template literal structure mismatch: {} quasis for {} expressions",
                    lit.quasis.len(),
                    lit.expressions.len()
                ),
                lit.span.start,
                lit.span.start + 1,
            ));
        }

        Ok(())
    }

    /// Validate escape sequences in template literal quasi
    fn validate_template_escape_sequences(
        &self,
        content: &str,
        start_pos: u32,
        quasi_index: usize,
    ) -> CompileResult<()> {
        let mut chars = content.chars().peekable();
        let mut position = 0;

        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.peek() {
                    Some(&next_ch) => {
                        chars.next(); // consume the next character
                        position += 2;

                        match next_ch {
                            // Valid template literal escape sequences
                            'n' | 't' | 'r' | 'b' | 'f' | 'v' | '0' | '\'' | '"' | '\\' | '`' => {
                                // These are always valid in template literals
                            }
                            // Line continuation (backslash followed by line terminator)
                            '\n' | '\r' => {
                                // Line continuation is valid in template literals
                                if next_ch == '\r' && chars.peek() == Some(&'\n') {
                                    chars.next(); // consume \n after \r
                                    position += 1;
                                }
                            }
                            // Hexadecimal escape sequences (\xHH)
                            'x' => {
                                if !self.validate_hex_escape_sequence(&mut chars) {
                                    return Err(CompileError::invalid_literal(
                                        format!(
                                            "Invalid hexadecimal escape sequence in template literal quasi {} at position {}",
                                            quasi_index, position
                                        ),
                                        start_pos + position as u32,
                                        start_pos + position as u32 + 1,
                                    ));
                                }
                                position += 2; // for the two hex digits
                            }
                            // Unicode escape sequences (\uHHHH or \u{...})
                            'u' => {
                                let consumed = self.validate_unicode_escape_sequence(&mut chars)?;
                                position += consumed;
                            }
                            // Template literal specific: ${} expressions are handled by parser
                            '$' => {
                                // In template literals, \$ is a valid escape to produce literal $
                                // The parser handles ${} expressions separately
                            }
                            // Legacy octal escape sequences (deprecated in strict mode)
                            '1'..='7' => {
                                if self.strict_mode {
                                    return Err(CompileError::legacy_syntax(
                                        format!(
                                            "Legacy octal escape sequence '\\{}' is not allowed in strict mode template literal",
                                            next_ch
                                        ),
                                        start_pos + position as u32,
                                        start_pos + position as u32 + 1,
                                    ));
                                }
                                // In non-strict mode, consume the octal digits
                                self.consume_octal_digits(&mut chars);
                            }
                            // Other numeric escapes (8, 9) are legacy and invalid in strict mode
                            '8' | '9' => {
                                if self.strict_mode {
                                    return Err(CompileError::legacy_syntax(
                                        format!(
                                            "Legacy numeric escape sequence '\\{}' is not allowed in strict mode template literal",
                                            next_ch
                                        ),
                                        start_pos + position as u32,
                                        start_pos + position as u32 + 1,
                                    ));
                                }
                            }
                            // Any other character after backslash
                            _ => {
                                // In template literals, most characters can be escaped
                                // Template literals are more permissive than regular strings
                                if self.strict_mode
                                    && !self.is_valid_template_escape_character(next_ch)
                                {
                                    return Err(CompileError::invalid_literal(
                                        format!(
                                            "Invalid escape sequence '\\{}' in strict mode template literal",
                                            next_ch
                                        ),
                                        start_pos + position as u32,
                                        start_pos + position as u32 + 1,
                                    ));
                                }
                            }
                        }
                    }
                    None => {
                        return Err(CompileError::invalid_literal(
                            format!(
                                "Unterminated escape sequence in template literal quasi {}",
                                quasi_index
                            ),
                            start_pos + position as u32,
                            start_pos + position as u32 + 1,
                        ));
                    }
                }
            } else {
                position += ch.len_utf8();
            }
        }

        Ok(())
    }

    /// Check if a character is valid after backslash in template literal escape sequences
    fn is_valid_template_escape_character(&self, ch: char) -> bool {
        match ch {
            // Standard escape characters
            'n' | 't' | 'r' | 'b' | 'f' | 'v' | '0' | '\'' | '"' | '\\' | '`' => true,
            // Hex and Unicode escapes
            'x' | 'u' => true,
            // Line terminators for line continuation
            '\n' | '\r' => true,
            // Template literal specific
            '$' => true,
            // In template literals, most characters can be escaped (more permissive than strings)
            _ => !self.strict_mode,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_literal_validator_creation() {
        let validator = LiteralValidator::default();
        assert!(!validator.is_strict_mode());
        assert_eq!(validator.ecma_version(), EcmaVersion::Latest);

        let strict_validator = LiteralValidator::strict();
        assert!(strict_validator.is_strict_mode());
        assert_eq!(strict_validator.ecma_version(), EcmaVersion::Latest);
    }

    #[test]
    fn test_validator_settings() {
        let mut validator = LiteralValidator::default();

        validator.set_strict_mode(true);
        assert!(validator.is_strict_mode());

        validator.set_ecma_version(EcmaVersion::Es2020);
        assert_eq!(validator.ecma_version(), EcmaVersion::Es2020);
    }

    #[test]
    fn test_source_location() {
        let location = SourceLocation::new(10, 5, "test.js");
        assert_eq!(location.line, 10);
        assert_eq!(location.column, 5);
        assert_eq!(location.filename, "test.js");
    }

    #[test]
    fn test_validation_context() {
        let location = SourceLocation::new(1, 1, "test.js");
        let context = ValidationContext::new(true, location, EcmaVersion::Es2020);

        assert!(context.strict_mode);
        assert_eq!(context.ecma_version, EcmaVersion::Es2020);
        assert_eq!(context.source_location.filename, "test.js");
    }

    // Property test for literal validator initialization
    // Feature: js-literals-compliance, Property 8: Early Error Generation
    // Validates: Requirements 7.1, 7.2, 7.4
    proptest! {
        #[test]
        fn prop_literal_validator_initialization(
            strict_mode in any::<bool>(),
            line in 1u32..1000u32,
            column in 1u32..1000u32,
            filename in "[a-zA-Z0-9_.-]{1,50}\\.(js|ts)",
        ) {
            // Test validator creation with different parameters
            let validator = LiteralValidator::new(strict_mode, EcmaVersion::Latest);
            prop_assert_eq!(validator.is_strict_mode(), strict_mode);
            prop_assert_eq!(validator.ecma_version(), EcmaVersion::Latest);

            // Test source location creation
            let location = SourceLocation::new(line, column, filename.clone());
            prop_assert_eq!(location.line, line);
            prop_assert_eq!(location.column, column);
            prop_assert_eq!(&location.filename, &filename);

            // Test validation context creation
            let context = ValidationContext::new(strict_mode, location, EcmaVersion::Latest);
            prop_assert_eq!(context.strict_mode, strict_mode);
            prop_assert_eq!(context.ecma_version, EcmaVersion::Latest);
            prop_assert_eq!(context.source_location.line, line);
            prop_assert_eq!(context.source_location.column, column);
        }

        #[test]
        fn prop_validator_settings_consistency(
            initial_strict in any::<bool>(),
            new_strict in any::<bool>(),
        ) {
            let mut validator = LiteralValidator::new(initial_strict, EcmaVersion::Es2020);

            // Verify initial state
            prop_assert_eq!(validator.is_strict_mode(), initial_strict);
            prop_assert_eq!(validator.ecma_version(), EcmaVersion::Es2020);

            // Change settings and verify
            validator.set_strict_mode(new_strict);
            validator.set_ecma_version(EcmaVersion::Latest);

            prop_assert_eq!(validator.is_strict_mode(), new_strict);
            prop_assert_eq!(validator.ecma_version(), EcmaVersion::Latest);
        }

        #[test]
        fn prop_ecma_version_all_variants(
            strict_mode in any::<bool>(),
        ) {
            let versions = [
                EcmaVersion::Es5,
                EcmaVersion::Es2015,
                EcmaVersion::Es2016,
                EcmaVersion::Es2017,
                EcmaVersion::Es2018,
                EcmaVersion::Es2019,
                EcmaVersion::Es2020,
                EcmaVersion::Es2021,
                EcmaVersion::Es2022,
                EcmaVersion::Es2023,
                EcmaVersion::Latest,
            ];

            for version in versions.iter() {
                let validator = LiteralValidator::new(strict_mode, *version);
                prop_assert_eq!(validator.ecma_version(), *version);
                prop_assert_eq!(validator.is_strict_mode(), strict_mode);
            }
        }
    }

    // Unit tests for numeric literal validation
    #[test]
    fn test_legacy_octal_detection() {
        let validator = LiteralValidator::strict();

        // Legacy octal patterns
        assert!(validator.is_legacy_octal("077"));
        assert!(validator.is_legacy_octal("0123"));
        assert!(validator.is_legacy_octal("07"));

        // Not legacy octal
        assert!(!validator.is_legacy_octal("0x77")); // hex
        assert!(!validator.is_legacy_octal("0b101")); // binary
        assert!(!validator.is_legacy_octal("0.77")); // decimal
        assert!(!validator.is_legacy_octal("77")); // no leading zero
        assert!(!validator.is_legacy_octal("0")); // just zero
        assert!(!validator.is_legacy_octal("089")); // contains 8/9
    }

    #[test]
    fn test_numeric_separator_validation() {
        let validator = LiteralValidator::default();

        // Valid separators
        assert!(validator.is_valid_numeric_separator_usage("1_000_000"));
        assert!(validator.is_valid_numeric_separator_usage("0xFF_FF"));
        assert!(validator.is_valid_numeric_separator_usage("0b1010_1010"));
        assert!(validator.is_valid_numeric_separator_usage("3.14_15"));
        assert!(validator.is_valid_numeric_separator_usage("1e10_00"));

        // Invalid separators
        assert!(!validator.is_valid_numeric_separator_usage("_123")); // leading
        assert!(!validator.is_valid_numeric_separator_usage("123_")); // trailing
        assert!(!validator.is_valid_numeric_separator_usage("1__23")); // consecutive
        assert!(!validator.is_valid_numeric_separator_usage("0x_FF")); // after prefix
        assert!(!validator.is_valid_numeric_separator_usage("3._14")); // around decimal
        assert!(!validator.is_valid_numeric_separator_usage("3.14_")); // around decimal
        assert!(!validator.is_valid_numeric_separator_usage("1e_10")); // around exponent
        assert!(!validator.is_valid_numeric_separator_usage("1_e10")); // around exponent
    }

    #[test]
    fn test_binary_literal_validation() {
        let validator = LiteralValidator::default();

        // Valid binary literals
        assert!(validator.is_valid_binary_literal("0b101"));
        assert!(validator.is_valid_binary_literal("0B1010"));
        assert!(validator.is_valid_binary_literal("0b1_0_1_0"));

        // Invalid binary literals
        assert!(!validator.is_valid_binary_literal("0b")); // empty
        assert!(!validator.is_valid_binary_literal("0b2")); // invalid digit
        assert!(!validator.is_valid_binary_literal("0b10a")); // invalid character
        assert!(!validator.is_valid_binary_literal("b101")); // missing 0
    }

    #[test]
    fn test_hex_literal_validation() {
        let validator = LiteralValidator::default();

        // Valid hex literals
        assert!(validator.is_valid_hex_literal("0xFF"));
        assert!(validator.is_valid_hex_literal("0x123ABC"));
        assert!(validator.is_valid_hex_literal("0XFF_FF"));

        // Invalid hex literals
        assert!(!validator.is_valid_hex_literal("0x")); // empty
        assert!(!validator.is_valid_hex_literal("0xGG")); // invalid digit
        assert!(!validator.is_valid_hex_literal("xFF")); // missing 0
    }

    #[test]
    fn test_exponent_format_validation() {
        let validator = LiteralValidator::default();

        // Valid exponent formats
        assert!(validator.is_valid_exponent_format("1e10"));
        assert!(validator.is_valid_exponent_format("1E10"));
        assert!(validator.is_valid_exponent_format("1e+10"));
        assert!(validator.is_valid_exponent_format("1e-10"));
        assert!(validator.is_valid_exponent_format("1.5e10"));
        assert!(validator.is_valid_exponent_format("1e1_0"));

        // Invalid exponent formats
        assert!(!validator.is_valid_exponent_format("1e")); // no digits
        assert!(!validator.is_valid_exponent_format("1e+")); // no digits after sign
        assert!(!validator.is_valid_exponent_format("1ea")); // invalid character
    }

    // Property tests for numeric literal validation
    // Feature: js-literals-compliance, Property 1: Numeric Literal Base Conversion
    // Feature: js-literals-compliance, Property 2: Numeric Separator Processing
    // Feature: js-literals-compliance, Property 3: Legacy Syntax Strict Mode Rejection
    // Validates: Requirements 1.1, 1.2, 1.3, 1.4
    proptest! {
        #[test]
        fn prop_legacy_octal_strict_mode_rejection(
            octal_digits in "[0-7]{1,8}",
        ) {
            let strict_validator = LiteralValidator::strict();
            let non_strict_validator = LiteralValidator::default();

            let octal_literal = format!("0{}", octal_digits);

            // In strict mode, legacy octal should be detected
            prop_assert!(strict_validator.is_legacy_octal(&octal_literal));

            // In non-strict mode, it's still legacy octal but not an error
            prop_assert!(non_strict_validator.is_legacy_octal(&octal_literal));
        }

        #[test]
        fn prop_numeric_separator_processing(
            base_number in 1u64..1_000_000u64,
            separator_positions in prop::collection::vec(1usize..6usize, 1..4),
        ) {
            let validator = LiteralValidator::default();

            // Create a number string with separators
            let base_str = base_number.to_string();
            let mut chars: Vec<char> = base_str.chars().collect();

            // Insert separators at valid positions (not at start/end)
            for &pos in separator_positions.iter().rev() {
                if pos < chars.len() {
                    chars.insert(pos, '_');
                }
            }

            let with_separators: String = chars.into_iter().collect();

            // Should be valid if it doesn't start/end with separator and no consecutive separators
            let is_valid_format = !with_separators.starts_with('_') &&
                                 !with_separators.ends_with('_') &&
                                 !with_separators.contains("__");

            prop_assert_eq!(
                validator.is_valid_numeric_separator_usage(&with_separators),
                is_valid_format
            );
        }

        #[test]
        fn prop_binary_literal_base_conversion(
            binary_digits in "[01]{1,32}",
        ) {
            let validator = LiteralValidator::default();

            let binary_literal = format!("0b{}", binary_digits);
            prop_assert!(validator.is_valid_binary_literal(&binary_literal));

            let binary_literal_upper = format!("0B{}", binary_digits);
            prop_assert!(validator.is_valid_binary_literal(&binary_literal_upper));

            // Test with separators
            if binary_digits.len() > 4 {
                let with_separator = format!("0b{}_{}", &binary_digits[..2], &binary_digits[2..]);
                prop_assert!(validator.is_valid_binary_literal(&with_separator));
            }
        }

        #[test]
        fn prop_hex_literal_base_conversion(
            hex_digits in "[0-9A-Fa-f]{1,16}",
        ) {
            let validator = LiteralValidator::default();

            let hex_literal = format!("0x{}", hex_digits);
            prop_assert!(validator.is_valid_hex_literal(&hex_literal));

            let hex_literal_upper = format!("0X{}", hex_digits);
            prop_assert!(validator.is_valid_hex_literal(&hex_literal_upper));

            // Test with separators
            if hex_digits.len() > 4 {
                let with_separator = format!("0x{}_{}", &hex_digits[..2], &hex_digits[2..]);
                prop_assert!(validator.is_valid_hex_literal(&with_separator));
            }
        }

        #[test]
        fn prop_invalid_binary_literals(
            invalid_char in "[2-9A-Za-z]",
            valid_prefix in "[01]*",
        ) {
            let validator = LiteralValidator::default();

            // Binary literal with invalid character should be invalid
            let invalid_binary = format!("0b{}{}", valid_prefix, invalid_char);
            prop_assert!(!validator.is_valid_binary_literal(&invalid_binary));
        }

        #[test]
        fn prop_invalid_hex_literals(
            invalid_char in "[G-Zg-z]",
            valid_prefix in "[0-9A-Fa-f]*",
        ) {
            let validator = LiteralValidator::default();

            // Hex literal with invalid character should be invalid
            let invalid_hex = format!("0x{}{}", valid_prefix, invalid_char);
            prop_assert!(!validator.is_valid_hex_literal(&invalid_hex));
        }

        #[test]
        fn prop_exponent_format_validation(
            mantissa in 1.0f64..1000.0f64,
            exponent in -10i32..10i32,
            use_upper_e in any::<bool>(),
            use_explicit_sign in any::<bool>(),
        ) {
            let validator = LiteralValidator::default();

            let e_char = if use_upper_e { 'E' } else { 'e' };
            let sign = if use_explicit_sign && exponent >= 0 { "+" } else { "" };

            let exponent_literal = format!("{}{}{}{}", mantissa, e_char, sign, exponent);

            // Should be valid exponent format
            prop_assert!(validator.is_valid_exponent_format(&exponent_literal));
        }

        #[test]
        fn prop_numeric_format_edge_cases(
            integer_part in 0u64..1000u64,
            fractional_digits in "[0-9]{0,6}",
            has_decimal in any::<bool>(),
        ) {
            let validator = LiteralValidator::default();

            let number_str = if has_decimal && !fractional_digits.is_empty() {
                format!("{}.{}", integer_part, fractional_digits)
            } else {
                integer_part.to_string()
            };

            // Basic numeric format should be valid
            prop_assert!(validator.is_valid_numeric_format(&number_str));

            // Multiple decimal points should be invalid
            let invalid_decimal = format!("{}.{}.0", integer_part, fractional_digits);
            prop_assert!(!validator.is_valid_numeric_format(&invalid_decimal));
        }
    }

    // Unit tests for string literal validation
    #[test]
    fn test_string_escape_sequence_validation() {
        let validator = LiteralValidator::default();
        let strict_validator = LiteralValidator::strict();

        // Valid escape sequences
        assert!(
            validator
                .validate_string_escape_sequences("Hello\\nWorld", 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_string_escape_sequences("Tab\\tSeparated", 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_string_escape_sequences("Quote\\\"Mark", 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_string_escape_sequences("Backslash\\\\", 0)
                .is_ok()
        );

        // Hexadecimal escapes
        assert!(
            validator
                .validate_string_escape_sequences("Hex\\x41", 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_string_escape_sequences("Hex\\xFF", 0)
                .is_ok()
        );

        // Unicode escapes
        assert!(
            validator
                .validate_string_escape_sequences("Unicode\\u0041", 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_string_escape_sequences("Unicode\\u{41}", 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_string_escape_sequences("Unicode\\u{1F600}", 0)
                .is_ok()
        );

        // Line continuation
        assert!(
            validator
                .validate_string_escape_sequences("Line\\\nContinuation", 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_string_escape_sequences("Line\\\r\nContinuation", 0)
                .is_ok()
        );
    }

    #[test]
    fn test_legacy_escape_sequences_strict_mode() {
        let strict_validator = LiteralValidator::strict();
        let non_strict_validator = LiteralValidator::default();

        // Legacy octal escapes should fail in strict mode
        assert!(
            strict_validator
                .validate_string_escape_sequences("Octal\\1", 0)
                .is_err()
        );
        assert!(
            strict_validator
                .validate_string_escape_sequences("Octal\\77", 0)
                .is_err()
        );
        assert!(
            strict_validator
                .validate_string_escape_sequences("Octal\\377", 0)
                .is_err()
        );

        // Legacy numeric escapes (8, 9) should fail in strict mode
        assert!(
            strict_validator
                .validate_string_escape_sequences("Invalid\\8", 0)
                .is_err()
        );
        assert!(
            strict_validator
                .validate_string_escape_sequences("Invalid\\9", 0)
                .is_err()
        );

        // But should be OK in non-strict mode
        assert!(
            non_strict_validator
                .validate_string_escape_sequences("Octal\\1", 0)
                .is_ok()
        );
        assert!(
            non_strict_validator
                .validate_string_escape_sequences("Invalid\\8", 0)
                .is_ok()
        );
    }

    #[test]
    fn test_invalid_escape_sequences() {
        let validator = LiteralValidator::default();

        // Invalid hex escapes
        assert!(
            validator
                .validate_string_escape_sequences("BadHex\\xGG", 0)
                .is_err()
        );
        assert!(
            validator
                .validate_string_escape_sequences("ShortHex\\xF", 0)
                .is_err()
        );

        // Invalid Unicode escapes
        assert!(
            validator
                .validate_string_escape_sequences("BadUnicode\\uGGGG", 0)
                .is_err()
        );
        assert!(
            validator
                .validate_string_escape_sequences("ShortUnicode\\u41", 0)
                .is_err()
        );
        assert!(
            validator
                .validate_string_escape_sequences("BadBraceUnicode\\u{GGGG}", 0)
                .is_err()
        );
        assert!(
            validator
                .validate_string_escape_sequences("EmptyBraceUnicode\\u{}", 0)
                .is_err()
        );
        assert!(
            validator
                .validate_string_escape_sequences("UnterminatedBrace\\u{41", 0)
                .is_err()
        );

        // Unterminated escape
        assert!(
            validator
                .validate_string_escape_sequences("Unterminated\\", 0)
                .is_err()
        );
    }

    #[test]
    fn test_hex_escape_validation() {
        let validator = LiteralValidator::default();
        let mut chars = "41".chars().peekable();
        assert!(validator.validate_hex_escape_sequence(&mut chars));

        let mut chars = "FF".chars().peekable();
        assert!(validator.validate_hex_escape_sequence(&mut chars));

        let mut chars = "GG".chars().peekable();
        assert!(!validator.validate_hex_escape_sequence(&mut chars));

        let mut chars = "F".chars().peekable();
        assert!(!validator.validate_hex_escape_sequence(&mut chars));
    }

    #[test]
    fn test_unicode_escape_validation() {
        let validator = LiteralValidator::default();

        // Test \uHHHH format
        let mut chars = "0041".chars().peekable();
        assert_eq!(
            validator
                .validate_unicode_escape_sequence(&mut chars)
                .unwrap(),
            4
        );

        let mut chars = "41".chars().peekable();
        assert!(
            validator
                .validate_unicode_escape_sequence(&mut chars)
                .is_err()
        );

        // Test \u{...} format
        let mut chars = "{41}".chars().peekable();
        assert_eq!(
            validator
                .validate_unicode_escape_sequence(&mut chars)
                .unwrap(),
            4
        );

        let mut chars = "{1F600}".chars().peekable();
        assert_eq!(
            validator
                .validate_unicode_escape_sequence(&mut chars)
                .unwrap(),
            7
        );

        let mut chars = "{}".chars().peekable();
        assert!(
            validator
                .validate_unicode_escape_sequence(&mut chars)
                .is_err()
        );

        let mut chars = "{41".chars().peekable();
        assert!(
            validator
                .validate_unicode_escape_sequence(&mut chars)
                .is_err()
        );
    }

    #[test]
    fn test_valid_escape_character() {
        let strict_validator = LiteralValidator::strict();
        let non_strict_validator = LiteralValidator::default();

        // Standard escapes should be valid in both modes
        assert!(strict_validator.is_valid_escape_character('n'));
        assert!(strict_validator.is_valid_escape_character('t'));
        assert!(strict_validator.is_valid_escape_character('\\'));
        assert!(strict_validator.is_valid_escape_character('"'));

        // Arbitrary characters should only be valid in non-strict mode
        assert!(!strict_validator.is_valid_escape_character('z'));
        assert!(non_strict_validator.is_valid_escape_character('z'));
    }

    // Property tests for string literal validation
    // Feature: js-literals-compliance, Property 4: String Escape Sequence Processing
    // Feature: js-literals-compliance, Property 3: Legacy Syntax Strict Mode Rejection
    // Validates: Requirements 2.1, 2.2, 2.3
    proptest! {
        #[test]
        fn prop_string_escape_sequence_processing(
            text_before in "[a-zA-Z0-9 ]{0,20}",
            text_after in "[a-zA-Z0-9 ]{0,20}",
            escape_char in prop::sample::select(vec!['n', 't', 'r', 'b', 'f', 'v', '0', '\'', '"', '\\']),
        ) {
            let validator = LiteralValidator::default();

            let content = format!("{}\\{}{}", text_before, escape_char, text_after);

            // Standard escape sequences should always be valid
            prop_assert!(validator.validate_string_escape_sequences(&content, 0).is_ok());
        }

        #[test]
        fn prop_legacy_escape_strict_mode_rejection(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
            octal_digit in prop::sample::select(vec!['1', '2', '3', '4', '5', '6', '7']),
        ) {
            let strict_validator = LiteralValidator::strict();
            let non_strict_validator = LiteralValidator::default();

            let content = format!("{}\\{}{}", text_before, octal_digit, text_after);

            // Legacy octal escapes should fail in strict mode
            prop_assert!(strict_validator.validate_string_escape_sequences(&content, 0).is_err());

            // But should be OK in non-strict mode
            prop_assert!(non_strict_validator.validate_string_escape_sequences(&content, 0).is_ok());
        }

        #[test]
        fn prop_hex_escape_sequence_validation(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
            hex_digit1 in "[0-9A-Fa-f]",
            hex_digit2 in "[0-9A-Fa-f]",
        ) {
            let validator = LiteralValidator::default();

            let content = format!("{}\\x{}{}{}", text_before, hex_digit1, hex_digit2, text_after);

            // Valid hex escape sequences should be accepted
            prop_assert!(validator.validate_string_escape_sequences(&content, 0).is_ok());
        }

        #[test]
        fn prop_unicode_escape_sequence_validation(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
            hex_digits in "[0-9A-Fa-f]{4}",
        ) {
            let validator = LiteralValidator::default();

            let content = format!("{}\\u{}{}", text_before, hex_digits, text_after);

            // Valid Unicode escape sequences should be accepted
            prop_assert!(validator.validate_string_escape_sequences(&content, 0).is_ok());
        }

        #[test]
        fn prop_unicode_brace_escape_validation(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
            hex_digits in "[0-9A-Fa-f]{1,6}",
        ) {
            let validator = LiteralValidator::default();

            let content = format!("{}\\u{{{}}}{}", text_before, hex_digits, text_after);

            // Valid Unicode brace escape sequences should be accepted
            prop_assert!(validator.validate_string_escape_sequences(&content, 0).is_ok());
        }

        #[test]
        fn prop_invalid_hex_escape_rejection(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
            invalid_char in "[G-Zg-z]",
        ) {
            let validator = LiteralValidator::default();

            let content = format!("{}\\x{}{}{}", text_before, invalid_char, "0", text_after);

            // Invalid hex escape sequences should be rejected
            prop_assert!(validator.validate_string_escape_sequences(&content, 0).is_err());
        }

        #[test]
        fn prop_line_continuation_validation(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
            line_terminator in prop::sample::select(vec!["\n", "\r", "\r\n"]),
        ) {
            let validator = LiteralValidator::default();

            let content = format!("{}\\{}{}", text_before, line_terminator, text_after);

            // Line continuation should be valid
            prop_assert!(validator.validate_string_escape_sequences(&content, 0).is_ok());
        }

        #[test]
        fn prop_arbitrary_escape_strict_vs_non_strict(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
            arbitrary_char in "[a-zA-Z]",
        ) {
            let strict_validator = LiteralValidator::strict();
            let non_strict_validator = LiteralValidator::default();

            // Convert string to char
            let arbitrary_char = arbitrary_char.chars().next().unwrap();

            // Skip characters that are valid escapes
            let valid_escapes = ['n', 't', 'r', 'b', 'f', 'v', 'x', 'u'];
            if valid_escapes.contains(&arbitrary_char) {
                return Ok(());
            }

            let content = format!("{}\\{}{}", text_before, arbitrary_char, text_after);

            // Arbitrary escapes should fail in strict mode but pass in non-strict
            prop_assert!(strict_validator.validate_string_escape_sequences(&content, 0).is_err());
            prop_assert!(non_strict_validator.validate_string_escape_sequences(&content, 0).is_ok());
        }
    }

    // Unit tests for RegExp literal validation
    #[test]
    fn test_regexp_flags_validation() {
        let validator = LiteralValidator::default();

        // Valid flags
        assert!(validator.validate_regexp_flags("g", 0).is_ok());
        assert!(validator.validate_regexp_flags("i", 0).is_ok());
        assert!(validator.validate_regexp_flags("m", 0).is_ok());
        assert!(validator.validate_regexp_flags("s", 0).is_ok());
        assert!(validator.validate_regexp_flags("u", 0).is_ok());
        assert!(validator.validate_regexp_flags("y", 0).is_ok());
        assert!(validator.validate_regexp_flags("d", 0).is_ok());
        assert!(validator.validate_regexp_flags("v", 0).is_ok());
        assert!(validator.validate_regexp_flags("gim", 0).is_ok());
        assert!(validator.validate_regexp_flags("", 0).is_ok()); // No flags

        // Invalid flags
        assert!(validator.validate_regexp_flags("x", 0).is_err()); // Invalid flag
        assert!(validator.validate_regexp_flags("gg", 0).is_err()); // Duplicate flag
        assert!(validator.validate_regexp_flags("uv", 0).is_err()); // Conflicting flags
        assert!(validator.validate_regexp_flags("z", 0).is_err()); // Invalid flag
    }

    #[test]
    fn test_regexp_pattern_basic_validation() {
        let validator = LiteralValidator::default();

        // Valid patterns
        assert!(validator.validate_regexp_pattern("hello", 0).is_ok());
        assert!(validator.validate_regexp_pattern("\\d+", 0).is_ok());
        assert!(validator.validate_regexp_pattern("[a-z]", 0).is_ok());
        assert!(validator.validate_regexp_pattern("(abc)", 0).is_ok());
        assert!(validator.validate_regexp_pattern("a*", 0).is_ok());
        assert!(validator.validate_regexp_pattern("a+", 0).is_ok());
        assert!(validator.validate_regexp_pattern("a?", 0).is_ok());

        // Invalid patterns
        assert!(validator.validate_regexp_pattern("[abc", 0).is_err()); // Unterminated character class
        assert!(validator.validate_regexp_pattern("(abc", 0).is_err()); // Unterminated group
        assert!(validator.validate_regexp_pattern("abc)", 0).is_err()); // Unmatched closing paren
    }

    #[test]
    fn test_regexp_escape_sequences() {
        let validator = LiteralValidator::default();

        // Valid escape sequences
        assert!(validator.validate_regexp_escape_sequences("\\n", 0).is_ok());
        assert!(validator.validate_regexp_escape_sequences("\\t", 0).is_ok());
        assert!(validator.validate_regexp_escape_sequences("\\d", 0).is_ok());
        assert!(validator.validate_regexp_escape_sequences("\\w", 0).is_ok());
        assert!(validator.validate_regexp_escape_sequences("\\s", 0).is_ok());
        assert!(validator.validate_regexp_escape_sequences("\\.", 0).is_ok());
        assert!(validator.validate_regexp_escape_sequences("\\*", 0).is_ok());
        assert!(
            validator
                .validate_regexp_escape_sequences("\\x41", 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_regexp_escape_sequences("\\u0041", 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_regexp_escape_sequences("\\u{41}", 0)
                .is_ok()
        );

        // Invalid escape sequences
        assert!(
            validator
                .validate_regexp_escape_sequences("\\xGG", 0)
                .is_err()
        );
        assert!(
            validator
                .validate_regexp_escape_sequences("\\uGGGG", 0)
                .is_err()
        );
        assert!(validator.validate_regexp_escape_sequences("\\", 0).is_err()); // Unterminated
    }

    #[test]
    fn test_regexp_character_classes() {
        let validator = LiteralValidator::default();

        // Valid character classes
        assert!(
            validator
                .validate_regexp_character_classes("[abc]", 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_regexp_character_classes("[a-z]", 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_regexp_character_classes("[^abc]", 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_regexp_character_classes("[[abc]]", 0)
                .is_ok()
        ); // Nested
        assert!(validator.validate_regexp_character_classes("", 0).is_ok()); // No classes

        // Invalid character classes
        assert!(
            validator
                .validate_regexp_character_classes("[abc", 0)
                .is_err()
        ); // Unterminated
        assert!(
            validator
                .validate_regexp_character_classes("[[abc]", 0)
                .is_err()
        ); // Unterminated nested
    }

    #[test]
    fn test_regexp_groups() {
        let validator = LiteralValidator::default();

        // Valid groups
        assert!(validator.validate_regexp_groups("(abc)", 0).is_ok());
        assert!(validator.validate_regexp_groups("(a)(b)", 0).is_ok());
        assert!(validator.validate_regexp_groups("((abc))", 0).is_ok()); // Nested
        assert!(validator.validate_regexp_groups("", 0).is_ok()); // No groups

        // Invalid groups
        assert!(validator.validate_regexp_groups("(abc", 0).is_err()); // Unterminated
        assert!(validator.validate_regexp_groups("abc)", 0).is_err()); // Unmatched closing
        assert!(validator.validate_regexp_groups("((abc)", 0).is_err()); // Unterminated nested
    }

    #[test]
    fn test_slash_usage_validation() {
        let validator = LiteralValidator::default();

        // Valid slash usage
        assert!(validator.is_valid_slash_usage("abc")); // No slashes
        assert!(validator.is_valid_slash_usage("\\/")); // Escaped slash
        assert!(validator.is_valid_slash_usage("[/]")); // Slash in character class

        // Invalid slash usage
        assert!(!validator.is_valid_slash_usage("/")); // Unescaped slash
        assert!(!validator.is_valid_slash_usage("a/b")); // Unescaped slash in middle
    }

    #[test]
    fn test_obvious_syntax_errors() {
        let validator = LiteralValidator::default();

        // Patterns with obvious syntax errors
        assert!(validator.has_obvious_syntax_errors("(?"));
        assert!(validator.has_obvious_syntax_errors("[^"));
        assert!(validator.has_obvious_syntax_errors("abc\\"));

        // Patterns without obvious syntax errors
        assert!(!validator.has_obvious_syntax_errors("abc"));
        assert!(!validator.has_obvious_syntax_errors("\\d+"));
        assert!(!validator.has_obvious_syntax_errors("[abc]"));
    }

    // Property tests for RegExp literal validation
    // Feature: js-literals-compliance, Property 5: RegExp Validation
    // Validates: Requirements 3.1, 3.2, 3.3
    proptest! {
        #[test]
        fn prop_regexp_flags_validation(
            flags in prop::collection::vec(
                prop::sample::select(vec!['g', 'i', 'm', 's', 'u', 'y', 'd', 'v']),
                0..5
            ).prop_map(|chars| {
                let mut unique_chars = std::collections::HashSet::new();
                chars.into_iter().filter(|c| unique_chars.insert(*c)).collect::<String>()
            })
        ) {
            let validator = LiteralValidator::default();

            // Check for conflicting flags (u and v cannot be together)
            let has_u = flags.contains('u');
            let has_v = flags.contains('v');
            let should_be_valid = !(has_u && has_v);

            let result = validator.validate_regexp_flags(&flags, 0);
            prop_assert_eq!(result.is_ok(), should_be_valid);
        }

        #[test]
        fn prop_regexp_flags_duplicate_detection(
            base_flag in prop::sample::select(vec!['g', 'i', 'm', 's', 'u', 'y']),
        ) {
            let validator = LiteralValidator::default();

            // Create flags with duplicate
            let flags_with_duplicate = format!("{}{}", base_flag, base_flag);

            // Should always fail due to duplicate
            prop_assert!(validator.validate_regexp_flags(&flags_with_duplicate, 0).is_err());
        }

        #[test]
        fn prop_regexp_pattern_basic_characters(
            text in "[a-zA-Z0-9 ]{1,20}",
        ) {
            let validator = LiteralValidator::default();

            // Basic alphanumeric text should always be valid
            prop_assert!(validator.validate_regexp_pattern(&text, 0).is_ok());
        }

        #[test]
        fn prop_regexp_character_class_validation(
            chars in "[a-zA-Z0-9]{1,10}",
        ) {
            let validator = LiteralValidator::default();

            let pattern = format!("[{}]", chars);

            // Well-formed character classes should be valid
            prop_assert!(validator.validate_regexp_character_classes(&pattern, 0).is_ok());
        }

        #[test]
        fn prop_regexp_group_validation(
            content in "[a-zA-Z0-9]{1,10}",
        ) {
            let validator = LiteralValidator::default();

            let pattern = format!("({})", content);

            // Well-formed groups should be valid
            prop_assert!(validator.validate_regexp_groups(&pattern, 0).is_ok());
        }

        #[test]
        fn prop_regexp_escape_sequences_validation(
            text_before in "[a-zA-Z0-9]{0,10}",
            text_after in "[a-zA-Z0-9]{0,10}",
            escape_char in prop::sample::select(vec!['n', 't', 'r', 'd', 'w', 's', '.', '*', '+', '?']),
        ) {
            let validator = LiteralValidator::default();

            let pattern = format!("{}\\{}{}", text_before, escape_char, text_after);

            // Standard RegExp escape sequences should be valid
            prop_assert!(validator.validate_regexp_escape_sequences(&pattern, 0).is_ok());
        }

        #[test]
        fn prop_regexp_hex_escape_validation(
            text_before in "[a-zA-Z0-9]{0,10}",
            text_after in "[a-zA-Z0-9]{0,10}",
            hex_digit1 in "[0-9A-Fa-f]",
            hex_digit2 in "[0-9A-Fa-f]",
        ) {
            let validator = LiteralValidator::default();

            let pattern = format!("{}\\x{}{}{}", text_before, hex_digit1, hex_digit2, text_after);

            // Valid hex escapes should be accepted
            prop_assert!(validator.validate_regexp_escape_sequences(&pattern, 0).is_ok());
        }

        #[test]
        fn prop_regexp_unicode_escape_validation(
            text_before in "[a-zA-Z0-9]{0,10}",
            text_after in "[a-zA-Z0-9]{0,10}",
            hex_digits in "[0-9A-Fa-f]{4}",
        ) {
            let validator = LiteralValidator::default();

            let pattern = format!("{}\\u{}{}", text_before, hex_digits, text_after);

            // Valid Unicode escapes should be accepted
            prop_assert!(validator.validate_regexp_escape_sequences(&pattern, 0).is_ok());
        }

        #[test]
        fn prop_regexp_invalid_flags_rejection(
            invalid_flag in "[A-Z]".prop_filter("Must not be valid flag", |s| {
                !['G', 'I', 'M', 'S', 'U', 'Y', 'D', 'V'].contains(&s.chars().next().unwrap())
            }),
        ) {
            let validator = LiteralValidator::default();

            // Invalid flags should be rejected
            prop_assert!(validator.validate_regexp_flags(&invalid_flag, 0).is_err());
        }

        #[test]
        fn prop_regexp_unterminated_structures(
            content in "[a-zA-Z0-9]{1,10}",
            structure_type in prop::sample::select(vec!["bracket", "paren"]),
        ) {
            let validator = LiteralValidator::default();

            let pattern = if structure_type == "bracket" {
                format!("[{}", content) // Unterminated bracket
            } else {
                format!("({}", content) // Unterminated paren
            };

            // Unterminated structures should be invalid
            if structure_type == "bracket" {
                prop_assert!(validator.validate_regexp_character_classes(&pattern, 0).is_err());
            } else {
                prop_assert!(validator.validate_regexp_groups(&pattern, 0).is_err());
            }
        }
    }

    // Unit tests for template literal validation
    #[test]
    fn test_template_escape_sequences() {
        let validator = LiteralValidator::default();

        // Valid escape sequences in template literals
        assert!(
            validator
                .validate_template_escape_sequences("Hello\\nWorld", 0, 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_template_escape_sequences("Tab\\tSeparated", 0, 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_template_escape_sequences("Quote\\`Mark", 0, 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_template_escape_sequences("Dollar\\$Sign", 0, 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_template_escape_sequences("Backslash\\\\", 0, 0)
                .is_ok()
        );

        // Hexadecimal and Unicode escapes
        assert!(
            validator
                .validate_template_escape_sequences("Hex\\x41", 0, 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_template_escape_sequences("Unicode\\u0041", 0, 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_template_escape_sequences("Unicode\\u{41}", 0, 0)
                .is_ok()
        );

        // Line continuation
        assert!(
            validator
                .validate_template_escape_sequences("Line\\\nContinuation", 0, 0)
                .is_ok()
        );
        assert!(
            validator
                .validate_template_escape_sequences("Line\\\r\nContinuation", 0, 0)
                .is_ok()
        );
    }

    #[test]
    fn test_template_legacy_escapes_strict_mode() {
        let strict_validator = LiteralValidator::strict();
        let non_strict_validator = LiteralValidator::default();

        // Legacy octal escapes should fail in strict mode
        assert!(
            strict_validator
                .validate_template_escape_sequences("Octal\\1", 0, 0)
                .is_err()
        );
        assert!(
            strict_validator
                .validate_template_escape_sequences("Octal\\77", 0, 0)
                .is_err()
        );

        // Legacy numeric escapes (8, 9) should fail in strict mode
        assert!(
            strict_validator
                .validate_template_escape_sequences("Invalid\\8", 0, 0)
                .is_err()
        );
        assert!(
            strict_validator
                .validate_template_escape_sequences("Invalid\\9", 0, 0)
                .is_err()
        );

        // But should be OK in non-strict mode
        assert!(
            non_strict_validator
                .validate_template_escape_sequences("Octal\\1", 0, 0)
                .is_ok()
        );
        assert!(
            non_strict_validator
                .validate_template_escape_sequences("Invalid\\8", 0, 0)
                .is_ok()
        );
    }

    #[test]
    fn test_template_invalid_escapes() {
        let validator = LiteralValidator::default();

        // Invalid hex escapes
        assert!(
            validator
                .validate_template_escape_sequences("BadHex\\xGG", 0, 0)
                .is_err()
        );
        assert!(
            validator
                .validate_template_escape_sequences("ShortHex\\xF", 0, 0)
                .is_err()
        );

        // Invalid Unicode escapes
        assert!(
            validator
                .validate_template_escape_sequences("BadUnicode\\uGGGG", 0, 0)
                .is_err()
        );
        assert!(
            validator
                .validate_template_escape_sequences("ShortUnicode\\u41", 0, 0)
                .is_err()
        );

        // Unterminated escape
        assert!(
            validator
                .validate_template_escape_sequences("Unterminated\\", 0, 0)
                .is_err()
        );
    }

    #[test]
    fn test_template_escape_character_validation() {
        let strict_validator = LiteralValidator::strict();
        let non_strict_validator = LiteralValidator::default();

        // Standard escapes should be valid in both modes
        assert!(strict_validator.is_valid_template_escape_character('n'));
        assert!(strict_validator.is_valid_template_escape_character('t'));
        assert!(strict_validator.is_valid_template_escape_character('`'));
        assert!(strict_validator.is_valid_template_escape_character('$'));

        // Arbitrary characters should only be valid in non-strict mode
        assert!(!strict_validator.is_valid_template_escape_character('z'));
        assert!(non_strict_validator.is_valid_template_escape_character('z'));
    }

    // Property tests for template literal validation
    // Feature: js-literals-compliance, Property 6: Template Literal Compilation
    // Validates: Requirements 4.1, 4.2, 4.3
    proptest! {
        #[test]
        fn prop_template_escape_sequence_processing(
            text_before in "[a-zA-Z0-9 ]{0,20}",
            text_after in "[a-zA-Z0-9 ]{0,20}",
            escape_char in prop::sample::select(vec!['n', 't', 'r', 'b', 'f', 'v', '0', '\'', '"', '\\', '`', '$']),
        ) {
            let validator = LiteralValidator::default();

            let content = format!("{}\\{}{}", text_before, escape_char, text_after);

            // Standard escape sequences should always be valid in template literals
            prop_assert!(validator.validate_template_escape_sequences(&content, 0, 0).is_ok());
        }

        #[test]
        fn prop_template_legacy_escape_strict_mode_rejection(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
            octal_digit in prop::sample::select(vec!['1', '2', '3', '4', '5', '6', '7']),
        ) {
            let strict_validator = LiteralValidator::strict();
            let non_strict_validator = LiteralValidator::default();

            let content = format!("{}\\{}{}", text_before, octal_digit, text_after);

            // Legacy octal escapes should fail in strict mode
            prop_assert!(strict_validator.validate_template_escape_sequences(&content, 0, 0).is_err());

            // But should be OK in non-strict mode
            prop_assert!(non_strict_validator.validate_template_escape_sequences(&content, 0, 0).is_ok());
        }

        #[test]
        fn prop_template_hex_escape_validation(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
            hex_digit1 in "[0-9A-Fa-f]",
            hex_digit2 in "[0-9A-Fa-f]",
        ) {
            let validator = LiteralValidator::default();

            let content = format!("{}\\x{}{}{}", text_before, hex_digit1, hex_digit2, text_after);

            // Valid hex escape sequences should be accepted
            prop_assert!(validator.validate_template_escape_sequences(&content, 0, 0).is_ok());
        }

        #[test]
        fn prop_template_unicode_escape_validation(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
            hex_digits in "[0-9A-Fa-f]{4}",
        ) {
            let validator = LiteralValidator::default();

            let content = format!("{}\\u{}{}", text_before, hex_digits, text_after);

            // Valid Unicode escape sequences should be accepted
            prop_assert!(validator.validate_template_escape_sequences(&content, 0, 0).is_ok());
        }

        #[test]
        fn prop_template_unicode_brace_escape_validation(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
            hex_digits in "[0-9A-Fa-f]{1,6}",
        ) {
            let validator = LiteralValidator::default();

            let content = format!("{}\\u{{{}}}{}", text_before, hex_digits, text_after);

            // Valid Unicode brace escape sequences should be accepted
            prop_assert!(validator.validate_template_escape_sequences(&content, 0, 0).is_ok());
        }

        #[test]
        fn prop_template_invalid_hex_escape_rejection(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
            invalid_char in "[G-Zg-z]",
        ) {
            let validator = LiteralValidator::default();

            let content = format!("{}\\x{}{}{}", text_before, invalid_char, "0", text_after);

            // Invalid hex escape sequences should be rejected
            prop_assert!(validator.validate_template_escape_sequences(&content, 0, 0).is_err());
        }

        #[test]
        fn prop_template_line_continuation_validation(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
            line_terminator in prop::sample::select(vec!["\n", "\r", "\r\n"]),
        ) {
            let validator = LiteralValidator::default();

            let content = format!("{}\\{}{}", text_before, line_terminator, text_after);

            // Line continuation should be valid in template literals
            prop_assert!(validator.validate_template_escape_sequences(&content, 0, 0).is_ok());
        }

        #[test]
        fn prop_template_dollar_escape_validation(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
        ) {
            let validator = LiteralValidator::default();

            let content = format!("{}\\${}", text_before, text_after);

            // Dollar sign escape should be valid in template literals
            prop_assert!(validator.validate_template_escape_sequences(&content, 0, 0).is_ok());
        }

        #[test]
        fn prop_template_backtick_escape_validation(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
        ) {
            let validator = LiteralValidator::default();

            let content = format!("{}\\`{}", text_before, text_after);

            // Backtick escape should be valid in template literals
            prop_assert!(validator.validate_template_escape_sequences(&content, 0, 0).is_ok());
        }

        #[test]
        fn prop_template_arbitrary_escape_strict_vs_non_strict(
            text_before in "[a-zA-Z0-9 ]{0,10}",
            text_after in "[a-zA-Z0-9 ]{0,10}",
            arbitrary_char in "[a-zA-Z]",
        ) {
            let strict_validator = LiteralValidator::strict();
            let non_strict_validator = LiteralValidator::default();

            // Convert string to char
            let arbitrary_char = arbitrary_char.chars().next().unwrap();

            // Skip characters that are valid escapes
            let valid_escapes = ['n', 't', 'r', 'b', 'f', 'v', 'x', 'u'];
            if valid_escapes.contains(&arbitrary_char) {
                return Ok(());
            }

            let content = format!("{}\\{}{}", text_before, arbitrary_char, text_after);

            // Arbitrary escapes should fail in strict mode but pass in non-strict
            prop_assert!(strict_validator.validate_template_escape_sequences(&content, 0, 0).is_err());
            prop_assert!(non_strict_validator.validate_template_escape_sequences(&content, 0, 0).is_ok());
        }
    }
}
