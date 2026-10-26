//! The head of an HTTP message, scanned in one pass over its bytes.
//!
//! The `http_parser` binding (`nodelib/compat/internal_http_parser.js`)
//! keeps llhttp's JavaScript-visible contract; this module is its scanner for
//! the part every message has: the request or status line and the header
//! block. One call reads the buffered bytes once and answers with the whole
//! head, where the binding used to cut, decode and match every line in
//! JavaScript.
//!
//! # Contents
//! - [`http_scan_cjs_value`] — the `__httpscan` module: `scanHead(stash,
//!   type, maxHeaderSize, maxHeaderPairs)`.
//!
//! # Invariants
//! - The answers, error codes and reasons are the binding's own: blank lines
//!   ahead of a request are skipped, a request's method token is checked as
//!   a prefix before its line completes, every line ends in CRLF (a lone CR
//!   is `HPE_LF_EXPECTED`, a bare LF `HPE_INVALID_HEADER_TOKEN`), the header
//!   block is bounded by `maxHeaderSize`, an obs-fold line continues the
//!   previous value, and at most `maxHeaderPairs` entries are kept.
//! - Text is the bytes read as Latin-1; values are trimmed of the
//!   whitespace `String.prototype.trim` removes in that range.
//! - The result is one of: a number, the blank-line bytes skipped while the
//!   head is incomplete; `[consumed, major, minor, methodOrStatus,
//!   urlOrReason, name0, value0, …]` for a complete head; or `[code, reason,
//!   offset, begun]` for a failure at byte `offset`, `begun` telling whether
//!   the start line already parsed.
//!
//! # See also
//! - `crates/otter-node/src/nodelib/compat/internal_http_parser.js`.

use otter_runtime::ValueWindow;
use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::{Local, NativeCtx, NativeScope, Value};

/// Request methods by index, as the binding's `methods` table orders them,
/// followed by the two only a request line may name.
const METHODS: [&str; 36] = [
    "DELETE",
    "GET",
    "HEAD",
    "POST",
    "PUT",
    "CONNECT",
    "OPTIONS",
    "TRACE",
    "COPY",
    "LOCK",
    "MKCOL",
    "MOVE",
    "PROPFIND",
    "PROPPATCH",
    "SEARCH",
    "UNLOCK",
    "BIND",
    "REBIND",
    "UNBIND",
    "ACL",
    "REPORT",
    "MKACTIVITY",
    "CHECKOUT",
    "MERGE",
    "M-SEARCH",
    "NOTIFY",
    "SUBSCRIBE",
    "UNSUBSCRIBE",
    "PATCH",
    "PURGE",
    "MKCALENDAR",
    "LINK",
    "UNLINK",
    "SOURCE",
    "PRI",
    "QUERY",
];

/// `HTTPParser.REQUEST`.
const REQUEST: i32 = 1;

/// CommonJS export: `scanHead`.
///
/// # Errors
/// Returns a native error when the export cannot be allocated or defined.
pub fn http_scan_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: Local<'scope>,
    _require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    let object = scope.object()?;
    let scan = scope.native_closure(
        "scanHead",
        4,
        &[],
        |ctx: &mut NativeCtx<'_>, args: ValueWindow<'_>, _captures: ValueWindow<'_>| {
            scan_head(ctx, &args)
        },
    )?;
    scope.set(object, "scanHead", scan)?;
    Ok(object)
}

/// One scanned head, or why it stops.
enum Scan {
    /// Incomplete: the blank-line bytes skipped so far.
    Wait(usize),
    /// A complete head ending at `consumed`.
    Head {
        consumed: usize,
        major: u32,
        minor: u32,
        /// The method index of a request, the status code of a response.
        selector: u32,
        /// The URL of a request, the reason phrase of a response.
        text: String,
        headers: Vec<String>,
    },
    /// A failure at byte `offset`; `begun` once the start line parsed.
    Error {
        code: &'static str,
        reason: &'static str,
        offset: usize,
        begun: bool,
    },
}

fn scan_head(ctx: &mut NativeCtx<'_>, args: &ValueWindow<'_>) -> Result<Value, NativeError> {
    let number = |index: usize| {
        args.get(index)
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0)
    };
    let kind = number(1) as i32;
    let max_header_size = number(2).max(0.0) as usize;
    let max_header_pairs = number(3);
    let Some(view) = args
        .first()
        .and_then(|value| value.as_typed_array(ctx.heap()))
    else {
        return Ok(Value::number_i32(0));
    };
    let heap = ctx.heap();
    let base = view.byte_offset(heap);
    let length = view.byte_length(heap);
    let scan = view.buffer(heap).with_bytes(heap, |bytes| {
        scan(
            &bytes[base..base + length],
            kind,
            max_header_size,
            max_header_pairs,
        )
    });
    ctx.scope(|mut scope| {
        let result = match scan {
            Scan::Wait(skipped) => scope.number(skipped as f64),
            Scan::Error {
                code,
                reason,
                offset,
                begun,
            } => {
                let array = scope.array(4)?;
                let code = scope.string(code)?;
                scope.set_index(array, 0, code)?;
                let reason = scope.string(reason)?;
                scope.set_index(array, 1, reason)?;
                let offset = scope.number(offset as f64);
                scope.set_index(array, 2, offset)?;
                let begun = scope.boolean(begun);
                scope.set_index(array, 3, begun)?;
                array
            }
            Scan::Head {
                consumed,
                major,
                minor,
                selector,
                text,
                headers,
            } => {
                let array = scope.array(5 + headers.len())?;
                for (index, value) in [
                    consumed as f64,
                    f64::from(major),
                    f64::from(minor),
                    f64::from(selector),
                ]
                .into_iter()
                .enumerate()
                {
                    let value = scope.number(value);
                    scope.set_index(array, index, value)?;
                }
                let text = scope.string(&text)?;
                scope.set_index(array, 4, text)?;
                for (index, header) in headers.iter().enumerate() {
                    let header = scope.string(header)?;
                    scope.set_index(array, 5 + index, header)?;
                }
                array
            }
        };
        Ok(scope.finish(result))
    })
}

/// Where the line starting at `start` ends: the index of its CR, `None`
/// when more bytes are needed, or the failure a stray CR or LF is.
fn line_end(bytes: &[u8], start: usize) -> Result<Option<usize>, (&'static str, &'static str)> {
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                return match bytes.get(index + 1) {
                    None => Ok(None),
                    Some(b'\n') => Ok(Some(index)),
                    Some(_) => Err(("HPE_LF_EXPECTED", "Expected LF after CR")),
                };
            }
            b'\n' => return Err(("HPE_INVALID_HEADER_TOKEN", "Invalid header token")),
            _ => index += 1,
        }
    }
    Ok(None)
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| char::from(byte)).collect()
}

/// `String.prototype.trim` over Latin-1 text.
fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| {
        matches!(c, '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}')
    })
}

/// The binding's header-name token: `^[\^_`a-zA-Z\-0-9!#$%&'*+.|~]+$`.
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'^' | b'_'
                        | b'`'
                        | b'-'
                        | b'!'
                        | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'.'
                        | b'|'
                        | b'~'
                )
        })
}

/// `/^HTTP\/(\d)\.(\d)/` at the start of `text`: the two digits.
fn http_version(text: &str) -> Option<(u32, u32)> {
    let bytes = text.as_bytes();
    if bytes.len() < 8 || &bytes[..5] != b"HTTP/" || bytes[6] != b'.' {
        return None;
    }
    let digit = |byte: u8| byte.is_ascii_digit().then(|| u32::from(byte - b'0'));
    Some((digit(bytes[5])?, digit(bytes[7])?))
}

fn scan(bytes: &[u8], kind: i32, max_header_size: usize, max_header_pairs: f64) -> Scan {
    let fail =
        |(code, reason): (&'static str, &'static str), offset: usize, begun: bool| Scan::Error {
            code,
            reason,
            offset,
            begun,
        };
    // The start line, after any blank lines ahead of a request.
    let mut start = 0;
    let line = loop {
        if start == bytes.len() {
            return Scan::Wait(start);
        }
        if kind == REQUEST && bytes[start] != b'\r' {
            let rest = &bytes[start..];
            let limit = rest.len().min(24);
            let end = rest[..limit]
                .iter()
                .position(|&byte| byte == b' ')
                .unwrap_or(limit);
            let token = latin1(&rest[..end]);
            if !METHODS
                .iter()
                .any(|method| method.starts_with(token.as_str()))
            {
                return fail(
                    ("HPE_INVALID_METHOD", "Invalid method encountered"),
                    start,
                    false,
                );
            }
        }
        match line_end(bytes, start) {
            Err(error) => return fail(error, start, false),
            Ok(None) => {
                if bytes.len() - start > max_header_size {
                    return fail(("HPE_HEADER_OVERFLOW", "Header overflow"), start, false);
                }
                return Scan::Wait(start);
            }
            Ok(Some(end)) if end == start => start += 2,
            Ok(Some(end)) => break (start, end),
        }
    };
    let (line_start, line_end_index) = line;
    let text = latin1(&bytes[line_start..line_end_index]);
    let (major, minor, selector, text) = if kind == REQUEST {
        let parts: Vec<&str> = text.split(' ').collect();
        if parts.len() < 3 {
            return fail(
                ("HPE_INVALID_METHOD", "Invalid method encountered"),
                line_start,
                false,
            );
        }
        let Some(method) = METHODS.iter().position(|method| *method == parts[0]) else {
            return fail(
                ("HPE_INVALID_METHOD", "Invalid method encountered"),
                line_start,
                false,
            );
        };
        let version = parts[parts.len() - 1];
        let Some((major, minor)) = http_version(version).filter(|_| version.len() == 8) else {
            return fail(
                ("HPE_INVALID_VERSION", "Invalid HTTP version"),
                line_start,
                false,
            );
        };
        let url = parts[1..parts.len() - 1].join(" ");
        (major, minor, method as u32, url)
    } else {
        // /^HTTP\/(\d)\.(\d) (\d{3})(?: (.*))?$/
        let parsed = http_version(&text).and_then(|(major, minor)| {
            let rest = text.get(8..)?;
            let rest = rest.strip_prefix(' ')?;
            let code = rest.get(..3)?;
            if !code.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let reason = match rest.get(3..)? {
                "" => String::new(),
                tail => tail.strip_prefix(' ')?.to_string(),
            };
            Some((major, minor, code.parse::<u32>().ok()?, reason))
        });
        let Some(parsed) = parsed else {
            return fail(
                ("HPE_INVALID_CONSTANT", "Expected HTTP/"),
                line_start,
                false,
            );
        };
        parsed
    };
    // The header block.
    let mut header_bytes = line_end_index - line_start + 2;
    let mut cursor = line_end_index + 2;
    let mut headers: Vec<String> = Vec::new();
    loop {
        let end = match line_end(bytes, cursor) {
            Err(error) => return fail(error, cursor, true),
            Ok(None) => {
                if header_bytes + (bytes.len() - cursor) > max_header_size {
                    return fail(("HPE_HEADER_OVERFLOW", "Header overflow"), cursor, true);
                }
                return Scan::Wait(start);
            }
            Ok(Some(end)) => end,
        };
        header_bytes += end - cursor + 2;
        if header_bytes > max_header_size {
            return fail(("HPE_HEADER_OVERFLOW", "Header overflow"), cursor, true);
        }
        let line = latin1(&bytes[cursor..end]);
        let line_start = cursor;
        cursor = end + 2;
        if line.is_empty() {
            return Scan::Head {
                consumed: cursor,
                major,
                minor,
                selector,
                text,
                headers,
            };
        }
        if line.starts_with([' ', '\t']) {
            let Some(last) = headers.last_mut() else {
                return fail(
                    ("HPE_INVALID_HEADER_TOKEN", "Invalid header token"),
                    line_start,
                    true,
                );
            };
            last.push(' ');
            last.push_str(trim(&line));
            continue;
        }
        let colon = match line.find(':') {
            Some(colon) if colon > 0 => colon,
            _ => {
                return fail(
                    ("HPE_INVALID_HEADER_TOKEN", "Invalid header token"),
                    line_start,
                    true,
                );
            }
        };
        let name = &line[..colon];
        if !is_token(name) {
            return fail(
                ("HPE_INVALID_HEADER_TOKEN", "Invalid header token"),
                line_start,
                true,
            );
        }
        if max_header_pairs <= 0.0 || (headers.len() as f64) < max_header_pairs {
            headers.push(name.to_string());
            headers.push(trim(&line[colon + 1..]).to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(bytes: &[u8]) -> (usize, u32, String, Vec<String>) {
        match scan(bytes, REQUEST, 16 * 1024, 0.0) {
            Scan::Head {
                consumed,
                selector,
                text,
                headers,
                ..
            } => (consumed, selector, text, headers),
            _ => panic!("expected a complete head"),
        }
    }

    #[test]
    fn a_request_head_scans_in_one_pass() {
        let (consumed, method, url, headers) =
            head(b"\r\nGET /a b HTTP/1.1\r\nHost:  x \r\nX-F: 1\r\n \tfold\r\n\r\nbody");
        assert_eq!(consumed, 50);
        assert_eq!(METHODS[method as usize], "GET");
        assert_eq!(url, "/a b");
        assert_eq!(headers, ["Host", "x", "X-F", "1 fold"]);
    }

    #[test]
    fn failures_carry_the_binding_codes_and_offsets() {
        assert!(matches!(
            scan(b"XYZ / HTTP/1.1\r\n", REQUEST, 1024, 0.0),
            Scan::Error {
                code: "HPE_INVALID_METHOD",
                ..
            }
        ));
        assert!(matches!(
            scan(b"GET / HTTP/1.1\r\nbad header\r\n", REQUEST, 1024, 0.0),
            Scan::Error {
                code: "HPE_INVALID_HEADER_TOKEN",
                offset: 16,
                begun: true,
                ..
            }
        ));
        assert!(matches!(
            scan(b"GET / HTTP/1.1\rX", REQUEST, 1024, 0.0),
            Scan::Error {
                code: "HPE_LF_EXPECTED",
                ..
            }
        ));
        assert!(matches!(
            scan(b"GET / HTTP/1.1\r\nHost: x\r\n", REQUEST, 1024, 0.0),
            Scan::Wait(0)
        ));
        match scan(b"HTTP/1.1 200 OK\r\nA: b\r\n\r\n", 2, 1024, 0.0) {
            Scan::Head { selector, text, .. } => {
                assert_eq!(selector, 200);
                assert_eq!(text, "OK");
            }
            _ => panic!("expected a status head"),
        }
    }
}
