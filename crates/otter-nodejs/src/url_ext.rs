//! Native `node:url` extension.
//!
//! Phase 1 focuses on the WHATWG `URL` constructor plus the Node helpers that
//! current official compatibility tests exercise directly.

use idna::{domain_to_ascii, domain_to_unicode};
use otter_macros::{js_class, js_method, js_static};
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::{NativeFn, Value};
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use std::path::{Path, PathBuf};
use url::{Host, Url};

const URL_BRAND_KEY: &str = "__otter_url_brand";
const LEGACY_URL_CTOR_KEY: &str = "__otter_legacy_url_ctor";
const HREF_KEY: &str = "__otter_url_href";
const PROTOCOL_KEY: &str = "__otter_url_protocol";
const USERNAME_KEY: &str = "__otter_url_username";
const PASSWORD_KEY: &str = "__otter_url_password";
const HOST_KEY: &str = "__otter_url_host";
const HOSTNAME_KEY: &str = "__otter_url_hostname";
const PORT_KEY: &str = "__otter_url_port";
const PATHNAME_KEY: &str = "__otter_url_pathname";
const SEARCH_KEY: &str = "__otter_url_search";
const HASH_KEY: &str = "__otter_url_hash";
const ORIGIN_KEY: &str = "__otter_url_origin";

const FILE_PATH_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}')
    .add(b'~');

// ---------------------------------------------------------------------------
// OtterExtension
// ---------------------------------------------------------------------------

pub struct NodeUrlExtension;

impl OtterExtension for NodeUrlExtension {
    fn name(&self) -> &str {
        "node_url"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 2] = [Profile::SafeCore, Profile::Full];
        &P
    }

    fn deps(&self) -> &[&str] {
        &[]
    }

    fn module_specifiers(&self) -> &[&str] {
        static S: [&str; 4] = ["node:url", "url", "internal/url", "node:internal/url"];
        &S
    }

    fn install(&self, ctx: &mut RegistrationContext) -> Result<(), VmError> {
        let url_ctor = build_url_class(ctx);
        let legacy_url_ctor = build_legacy_url_class(ctx);
        ctx.global_value("URL", url_ctor);
        set_hidden_value(&ctx.global(), LEGACY_URL_CTOR_KEY, legacy_url_ctor);
        Ok(())
    }

    fn load_module(
        &self,
        specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        if specifier == "internal/url" {
            let (name, func, length) = InternalUrlModule::is_url_decl();
            return Some(ctx.module_namespace().function(name, func, length).build());
        }

        let url_ctor = ctx
            .global()
            .get(&PropertyKey::string("URL"))
            .unwrap_or(Value::undefined());
        let legacy_url_ctor = ctx
            .global()
            .get(&PropertyKey::string(LEGACY_URL_CTOR_KEY))
            .unwrap_or(Value::undefined());

        let mut ns = ctx
            .module_namespace()
            .property("URL", url_ctor)
            .property("Url", legacy_url_ctor);

        for decl in module_decls() {
            let (name, func, length) = decl();
            ns = ns.function(name, func, length);
        }

        Some(ns.build())
    }
}

pub fn node_url_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeUrlExtension)
}

// ---------------------------------------------------------------------------
// WHATWG URL class
// ---------------------------------------------------------------------------

#[js_class(name = "URL")]
pub struct JsUrl;

#[js_class]
impl JsUrl {
    #[js_method(constructor)]
    pub fn constructor(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        if !ncx.is_construct() {
            return Err(VmError::type_error("Constructor URL requires 'new'"));
        }

        let input = primitive_string_arg(args.first(), "input")?;
        let base = resolve_base_arg(args.get(1))?;

        let parsed = parse_url_input(&input, base.as_deref())?;
        let this_obj = this
            .as_object()
            .ok_or_else(|| VmError::type_error("URL constructor receiver must be an object"))?;
        write_url_state(&this_obj, &parsed);

        Ok(Value::undefined())
    }

    #[js_method(name = "href", kind = "getter")]
    pub fn href(this: &Value, _args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
        url_prop(this, HREF_KEY)
    }

    #[js_method(name = "protocol", kind = "getter")]
    pub fn protocol(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        url_prop(this, PROTOCOL_KEY)
    }

    #[js_method(name = "username", kind = "getter")]
    pub fn username(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        url_prop(this, USERNAME_KEY)
    }

    #[js_method(name = "password", kind = "getter")]
    pub fn password(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        url_prop(this, PASSWORD_KEY)
    }

    #[js_method(name = "host", kind = "getter")]
    pub fn host(this: &Value, _args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
        url_prop(this, HOST_KEY)
    }

    #[js_method(name = "hostname", kind = "getter")]
    pub fn hostname(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        url_prop(this, HOSTNAME_KEY)
    }

    #[js_method(name = "port", kind = "getter")]
    pub fn port(this: &Value, _args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
        url_prop(this, PORT_KEY)
    }

    #[js_method(name = "pathname", kind = "getter")]
    pub fn pathname(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        url_prop(this, PATHNAME_KEY)
    }

    #[js_method(name = "search", kind = "getter")]
    pub fn search(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        url_prop(this, SEARCH_KEY)
    }

    #[js_method(name = "hash", kind = "getter")]
    pub fn hash(this: &Value, _args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
        url_prop(this, HASH_KEY)
    }

    #[js_method(name = "origin", kind = "getter")]
    pub fn origin(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        url_prop(this, ORIGIN_KEY)
    }

    #[js_method(name = "toString", length = 0)]
    pub fn to_string(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        url_prop(this, HREF_KEY)
    }

    #[js_method(name = "toJSON", length = 0)]
    pub fn to_json(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        url_prop(this, HREF_KEY)
    }

    #[js_static(name = "revokeObjectURL", length = 1)]
    pub fn revoke_object_url(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        if args.is_empty() {
            return Err(VmError::type_error(
                "The \"url\" argument is required for URL.revokeObjectURL()",
            ));
        }
        Ok(Value::undefined())
    }
}

fn build_url_class(ctx: &RegistrationContext) -> Value {
    let mut builder = ctx
        .builtin_fresh("URL")
        .constructor_fn(JsUrl::constructor, 1);

    let accessors: &[(&str, fn() -> (&'static str, NativeFn, u32))] = &[
        ("href", JsUrl::href_decl),
        ("protocol", JsUrl::protocol_decl),
        ("username", JsUrl::username_decl),
        ("password", JsUrl::password_decl),
        ("host", JsUrl::host_decl),
        ("hostname", JsUrl::hostname_decl),
        ("port", JsUrl::port_decl),
        ("pathname", JsUrl::pathname_decl),
        ("search", JsUrl::search_decl),
        ("hash", JsUrl::hash_decl),
        ("origin", JsUrl::origin_decl),
    ];

    for (name, decl) in accessors {
        let (_, getter, _) = decl();
        builder = builder.accessor(name, Some(getter), None);
    }

    for decl in [JsUrl::to_string_decl, JsUrl::to_json_decl] {
        let (name, func, length) = decl();
        builder = builder.method_native(name, func, length);
    }

    let (name, func, length) = JsUrl::revoke_object_url_decl();
    builder = builder.static_method_native(name, func, length);

    builder.build()
}

// ---------------------------------------------------------------------------
// Legacy node:url surface
// ---------------------------------------------------------------------------

#[js_class(name = "Url")]
pub struct LegacyUrl;

#[js_class]
impl LegacyUrl {
    #[js_method(constructor)]
    pub fn constructor(
        this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let obj = this
            .as_object()
            .ok_or_else(|| VmError::type_error("Url constructor receiver must be an object"))?;
        init_legacy_url_defaults(&obj);
        Ok(Value::undefined())
    }

    #[js_method(name = "resolve", length = 1)]
    pub fn resolve(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let base = legacy_state_from_receiver(this)?;
        let relative = legacy_state_from_value(args.first(), ncx)?;
        let resolved = legacy_resolve(&base, &relative);
        Ok(Value::string(JsString::new_gc(
            &preserve_file_slash_style_for_state(&base, &resolved),
        )))
    }

    #[js_method(name = "resolveObject", length = 1)]
    pub fn resolve_object(
        this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let base = legacy_state_from_receiver(this)?;
        let relative = legacy_state_from_value(args.first(), ncx)?;
        let resolved = legacy_resolve(&base, &relative);
        legacy_parse_to_value(
            ncx,
            &preserve_file_slash_style_for_state(&base, &resolved),
            false,
        )
    }
}

fn build_legacy_url_class(ctx: &RegistrationContext) -> Value {
    ctx.builtin_fresh("Url")
        .constructor_fn(LegacyUrl::constructor, 0)
        .build()
}

#[js_class(name = "NodeUrlModule")]
pub struct NodeUrlModule;

#[js_class]
impl NodeUrlModule {
    #[js_static(name = "resolve", length = 2)]
    pub fn resolve(
        _this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let source = primitive_string_arg(args.first(), "source")?;
        let relative = primitive_string_arg(args.get(1), "relative")?;
        let source_state = legacy_state_from_input(ncx, &source)?;
        let relative_state = legacy_state_from_input(ncx, &relative)?;
        let resolved = legacy_resolve(&source_state, &relative_state);
        Ok(Value::string(JsString::new_gc(&preserve_file_slash_style(
            &source, &resolved,
        ))))
    }

    #[js_static(name = "resolveObject", length = 2)]
    pub fn resolve_object(
        _this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let source = args.first().cloned().unwrap_or(Value::undefined());
        if !source.to_boolean() {
            return Ok(args.get(1).cloned().unwrap_or(Value::undefined()));
        }

        let source_state = legacy_state_from_source_value(&source, ncx)?;
        let relative_state = legacy_state_from_value(args.get(1), ncx)?;
        let resolved = legacy_resolve(&source_state, &relative_state);
        legacy_parse_to_value(
            ncx,
            &preserve_file_slash_style_for_state(&source_state, &resolved),
            false,
        )
    }

    #[js_static(name = "pathToFileURL", length = 1)]
    pub fn path_to_file_url(
        _this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let raw_path = primitive_string_arg(args.first(), "path")?;
        let windows = parse_windows_option(args.get(1))?.unwrap_or(false);
        let href = if windows {
            path_to_file_href_windows(&raw_path)?
        } else {
            path_to_file_href_posix(&raw_path)?
        };
        build_url_value(
            ncx,
            &Url::parse(&href).map_err(|e| VmError::type_error(e.to_string()))?,
        )
    }

    #[js_static(name = "fileURLToPath", length = 1)]
    pub fn file_url_to_path(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let href = file_url_input(args.first())?;
        let windows = parse_windows_option(args.get(1))?.unwrap_or(false);
        let parsed = Url::parse(&href).map_err(|_| VmError::type_error("Invalid URL"))?;
        if parsed.scheme() != "file" {
            return Err(VmError::type_error("The URL must be of scheme file"));
        }

        let path = if windows {
            file_url_to_windows_path(&parsed)?
        } else {
            file_url_to_posix_path(&parsed)?
        };

        Ok(Value::string(JsString::new_gc(&path)))
    }

    #[js_static(name = "urlToHttpOptions", length = 1)]
    pub fn url_to_http_options(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let value = args.first().cloned().unwrap_or(Value::undefined());
        let obj = value
            .as_object()
            .ok_or_else(|| VmError::type_error("The \"url\" argument must be of type object"))?;

        let out = GcRef::new(JsObject::new(Value::null()));
        let (protocol, username, password, hostname, port, pathname, search, hash, href): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = if is_branded_url_object(&obj) {
            (
                get_string_prop(&obj, PROTOCOL_KEY),
                get_string_prop(&obj, USERNAME_KEY),
                get_string_prop(&obj, PASSWORD_KEY),
                get_string_prop(&obj, HOSTNAME_KEY),
                get_string_prop(&obj, PORT_KEY),
                get_string_prop(&obj, PATHNAME_KEY),
                get_string_prop(&obj, SEARCH_KEY),
                get_string_prop(&obj, HASH_KEY),
                get_string_prop(&obj, HREF_KEY),
            )
        } else {
            (
                value_as_string(obj.get(&PropertyKey::string("protocol"))),
                value_as_string(obj.get(&PropertyKey::string("username"))),
                value_as_string(obj.get(&PropertyKey::string("password"))),
                value_as_string(obj.get(&PropertyKey::string("hostname"))),
                value_as_string(obj.get(&PropertyKey::string("port"))),
                value_as_string(obj.get(&PropertyKey::string("pathname"))),
                value_as_string(obj.get(&PropertyKey::string("search"))),
                value_as_string(obj.get(&PropertyKey::string("hash"))),
                value_as_string(obj.get(&PropertyKey::string("href"))),
            )
        };

        set_plain_prop(
            &out,
            "protocol",
            protocol
                .map(|s| Value::string(JsString::new_gc(&s)))
                .unwrap_or(Value::undefined()),
        );

        let auth = match (username.as_deref(), password.as_deref()) {
            (Some(""), Some("")) | (Some(""), None) | (None, _) => Value::undefined(),
            (Some(user), Some(pass)) if !pass.is_empty() => {
                Value::string(JsString::new_gc(&format!("{user}:{pass}")))
            }
            (Some(user), _) => Value::string(JsString::new_gc(user)),
        };
        set_plain_prop(&out, "auth", auth);

        set_plain_prop(
            &out,
            "hostname",
            hostname
                .map(|s| Value::string(JsString::new_gc(&s)))
                .unwrap_or(Value::undefined()),
        );

        let port_value = port
            .and_then(|p| p.parse::<f64>().ok())
            .map(Value::number)
            .unwrap_or_else(|| Value::number(f64::NAN));
        set_plain_prop(&out, "port", port_value);

        let path = format!(
            "{}{}",
            pathname.clone().unwrap_or_default(),
            search.clone().unwrap_or_default()
        );
        set_plain_prop(&out, "path", Value::string(JsString::new_gc(&path)));

        set_plain_prop(
            &out,
            "pathname",
            pathname
                .map(|s| Value::string(JsString::new_gc(&s)))
                .unwrap_or(Value::undefined()),
        );
        set_plain_prop(
            &out,
            "search",
            search
                .map(|s| Value::string(JsString::new_gc(&s)))
                .unwrap_or(Value::undefined()),
        );
        set_plain_prop(
            &out,
            "hash",
            hash.map(|s| Value::string(JsString::new_gc(&s)))
                .unwrap_or(Value::undefined()),
        );
        set_plain_prop(
            &out,
            "href",
            href.map(|s| Value::string(JsString::new_gc(&s)))
                .unwrap_or(Value::undefined()),
        );

        Ok(Value::object(out))
    }

    #[js_static(name = "format", length = 1)]
    pub fn format(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let target = args.first().cloned().unwrap_or(Value::undefined());
        let options = parse_format_options(args.get(1))?;

        if target.is_undefined()
            || target.is_null()
            || target.is_boolean()
            || target.is_number()
            || target.is_callable()
            || target.is_symbol()
        {
            return Err(VmError::type_error("The \"urlObject\" argument is invalid"));
        }

        if let Some(s) = target.as_string() {
            return Ok(Value::string(JsString::new_gc(s.as_str())));
        }

        let Some(obj) = target.as_object() else {
            return Err(VmError::type_error("The \"urlObject\" argument is invalid"));
        };

        let formatted = if is_branded_url_object(&obj) {
            format_branded_url(&obj, &options)
        } else {
            format_plain_url_object(&obj)
        };

        Ok(Value::string(JsString::new_gc(&formatted)))
    }

    #[js_static(name = "domainToASCII", length = 1)]
    pub fn domain_to_ascii_fn(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let input = primitive_string_arg(args.first(), "domain")?;
        let output = domain_to_ascii(&input).unwrap_or_default();
        Ok(Value::string(JsString::new_gc(&output)))
    }

    #[js_static(name = "domainToUnicode", length = 1)]
    pub fn domain_to_unicode_fn(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let input = primitive_string_arg(args.first(), "domain")?;
        let (output, _) = domain_to_unicode(&input);
        Ok(Value::string(JsString::new_gc(&output)))
    }

    #[js_static(name = "parse", length = 1)]
    pub fn parse(_this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
        let input = primitive_string_arg(args.first(), "url")?;
        let parse_query = args.get(1).map(|v| v.to_boolean()).unwrap_or(false);
        let out = legacy_url_instance(ncx)?;
        let obj_proto = ncx
            .ctx
            .realm_intrinsics(ncx.ctx.realm_id())
            .map(|intrinsics| intrinsics.object_prototype)
            .ok_or_else(|| VmError::type_error("Object.prototype is not available"))?;
        populate_legacy_parse(&out, &input, parse_query, obj_proto);
        Ok(Value::object(out))
    }
}

#[js_class(name = "InternalUrlModule")]
pub struct InternalUrlModule;

#[js_class]
impl InternalUrlModule {
    #[js_static(name = "isURL", length = 1)]
    pub fn is_url(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let result = args
            .first()
            .and_then(|v| v.as_object())
            .map(|obj| is_branded_url_object(&obj))
            .unwrap_or(false);
        Ok(Value::boolean(result))
    }
}

fn module_decls() -> &'static [fn() -> (&'static str, NativeFn, u32)] {
    &[
        NodeUrlModule::resolve_decl,
        NodeUrlModule::resolve_object_decl,
        NodeUrlModule::path_to_file_url_decl,
        NodeUrlModule::file_url_to_path_decl,
        NodeUrlModule::url_to_http_options_decl,
        NodeUrlModule::format_decl,
        NodeUrlModule::domain_to_ascii_fn_decl,
        NodeUrlModule::domain_to_unicode_fn_decl,
        NodeUrlModule::parse_decl,
    ]
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FormatOptions {
    auth: bool,
    fragment: bool,
    search: bool,
    unicode: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LegacyUrlState {
    protocol: Option<String>,
    slashes: bool,
    auth: Option<String>,
    host: Option<String>,
    port: Option<String>,
    hostname: Option<String>,
    hash: Option<String>,
    search: Option<String>,
    query: Option<String>,
    pathname: Option<String>,
    path: Option<String>,
    href: String,
}

fn parse_format_options(value: Option<&Value>) -> Result<FormatOptions, VmError> {
    let Some(value) = value else {
        return Ok(FormatOptions {
            auth: true,
            fragment: true,
            search: true,
            unicode: false,
        });
    };

    if value.is_undefined() || value.is_null() {
        return Ok(FormatOptions {
            auth: true,
            fragment: true,
            search: true,
            unicode: false,
        });
    }

    let obj = value
        .as_object()
        .ok_or_else(|| VmError::type_error("The \"options\" argument must be of type object"))?;

    Ok(FormatOptions {
        auth: obj
            .get(&PropertyKey::string("auth"))
            .map(|v| v.to_boolean())
            .unwrap_or(true),
        fragment: obj
            .get(&PropertyKey::string("fragment"))
            .map(|v| v.to_boolean())
            .unwrap_or(true),
        search: obj
            .get(&PropertyKey::string("search"))
            .map(|v| v.to_boolean())
            .unwrap_or(true),
        unicode: obj
            .get(&PropertyKey::string("unicode"))
            .map(|v| v.to_boolean())
            .unwrap_or(false),
    })
}

fn format_branded_url(obj: &GcRef<JsObject>, options: &FormatOptions) -> String {
    let protocol = get_string_prop(obj, PROTOCOL_KEY).unwrap_or_default();
    let username = get_string_prop(obj, USERNAME_KEY).unwrap_or_default();
    let password = get_string_prop(obj, PASSWORD_KEY).unwrap_or_default();
    let pathname = get_string_prop(obj, PATHNAME_KEY).unwrap_or_default();
    let search = get_string_prop(obj, SEARCH_KEY).unwrap_or_default();
    let hash = get_string_prop(obj, HASH_KEY).unwrap_or_default();
    let port = get_string_prop(obj, PORT_KEY).unwrap_or_default();
    let mut hostname = get_string_prop(obj, HOSTNAME_KEY).unwrap_or_default();

    if options.unicode && !hostname.is_empty() && !hostname.starts_with('[') {
        hostname = domain_to_unicode(&hostname).0;
    }

    let host = if port.is_empty() {
        hostname
    } else {
        format!("{hostname}:{port}")
    };

    let mut out = String::new();
    out.push_str(&protocol);

    if !host.is_empty() {
        out.push_str("//");
        if options.auth && !username.is_empty() {
            out.push_str(&username);
            if !password.is_empty() {
                out.push(':');
                out.push_str(&password);
            }
            out.push('@');
        }
        out.push_str(&host);
    }

    out.push_str(&pathname);

    if options.search {
        out.push_str(&search);
    }
    if options.fragment {
        out.push_str(&hash);
    }

    out
}

fn format_plain_url_object(obj: &GcRef<JsObject>) -> String {
    let protocol = value_as_string(obj.get(&PropertyKey::string("protocol"))).unwrap_or_default();
    let host = value_as_string(obj.get(&PropertyKey::string("host"))).unwrap_or_default();
    let pathname = value_as_string(obj.get(&PropertyKey::string("pathname"))).unwrap_or_default();
    let search = value_as_string(obj.get(&PropertyKey::string("search"))).unwrap_or_default();
    let hash = value_as_string(obj.get(&PropertyKey::string("hash"))).unwrap_or_default();
    let href = value_as_string(obj.get(&PropertyKey::string("href"))).unwrap_or_default();

    if !href.is_empty() {
        return href;
    }
    if protocol.is_empty()
        && host.is_empty()
        && pathname.is_empty()
        && search.is_empty()
        && hash.is_empty()
    {
        return String::new();
    }

    let mut out = String::new();
    out.push_str(&protocol);
    if !host.is_empty() {
        if !protocol.ends_with("//") {
            out.push_str("//");
        }
        out.push_str(&host);
    }
    out.push_str(&pathname);
    out.push_str(&search);
    out.push_str(&hash);
    out
}

fn legacy_state_from_receiver(this: &Value) -> Result<LegacyUrlState, VmError> {
    let obj = this
        .as_object()
        .ok_or_else(|| VmError::type_error("Url method called on non-object"))?;
    Ok(LegacyUrlState::from_object(&obj))
}

fn legacy_state_from_source_value(
    value: &Value,
    ncx: &NativeContext,
) -> Result<LegacyUrlState, VmError> {
    if let Some(s) = value.as_string() {
        return legacy_state_from_input(ncx, s.as_str());
    }
    if let Some(obj) = value.as_object() {
        return Ok(LegacyUrlState::from_object(&obj));
    }
    Err(VmError::type_error(
        "The \"source\" argument must be a string or Url object",
    ))
}

fn legacy_state_from_value(
    value: Option<&Value>,
    ncx: &NativeContext,
) -> Result<LegacyUrlState, VmError> {
    let default_value = Value::undefined();
    let value = value.unwrap_or(&default_value);
    if let Some(s) = value.as_string() {
        return legacy_state_from_input(ncx, s.as_str());
    }
    if let Some(obj) = value.as_object() {
        return Ok(LegacyUrlState::from_object(&obj));
    }
    Err(VmError::type_error(
        "The \"relative\" argument must be a string or Url object",
    ))
}

fn legacy_state_from_input(ncx: &NativeContext, input: &str) -> Result<LegacyUrlState, VmError> {
    if let Some(state) = parse_legacy_resolve_input(input) {
        return Ok(state);
    }
    let out = legacy_url_instance(ncx)?;
    let obj_proto = object_prototype(ncx)?;
    populate_legacy_parse(&out, input, false, obj_proto);
    Ok(LegacyUrlState::from_object(&out))
}

fn legacy_parse_to_value(
    ncx: &NativeContext,
    input: &str,
    parse_query: bool,
) -> Result<Value, VmError> {
    let out = legacy_url_instance(ncx)?;
    let obj_proto = object_prototype(ncx)?;
    populate_legacy_parse(&out, input, parse_query, obj_proto);
    Ok(Value::object(out))
}

fn object_prototype(ncx: &NativeContext) -> Result<GcRef<JsObject>, VmError> {
    ncx.ctx
        .realm_intrinsics(ncx.ctx.realm_id())
        .map(|intrinsics| intrinsics.object_prototype)
        .ok_or_else(|| VmError::type_error("Object.prototype is not available"))
}

impl LegacyUrlState {
    fn from_object(obj: &GcRef<JsObject>) -> Self {
        Self {
            protocol: nullable_string_prop(obj, "protocol"),
            slashes: obj
                .get(&PropertyKey::string("slashes"))
                .filter(|v| !v.is_null() && !v.is_undefined())
                .map(|v| v.to_boolean())
                .unwrap_or(false),
            auth: nullable_string_prop(obj, "auth"),
            host: nullable_string_prop(obj, "host"),
            port: nullable_string_prop(obj, "port"),
            hostname: nullable_string_prop(obj, "hostname"),
            hash: nullable_string_prop(obj, "hash"),
            search: nullable_string_prop(obj, "search"),
            query: nullable_string_prop(obj, "query"),
            pathname: nullable_string_prop(obj, "pathname"),
            path: nullable_string_prop(obj, "path"),
            href: nullable_string_prop(obj, "href").unwrap_or_default(),
        }
    }
}

fn nullable_string_prop(obj: &GcRef<JsObject>, key: &str) -> Option<String> {
    match obj.get(&PropertyKey::string(key)) {
        Some(v) if v.is_null() || v.is_undefined() => None,
        Some(v) => v.as_string().map(|s| s.as_str().to_string()),
        _ => None,
    }
}

fn legacy_resolve(source: &LegacyUrlState, relative: &LegacyUrlState) -> String {
    let mut relative = relative.clone();
    let mut result = source.clone();

    result.hash = relative.hash.clone();

    if relative.href.is_empty() {
        result.href = legacy_format_state(&result);
        return result.href.clone();
    }

    if relative.slashes && relative.protocol.is_none() {
        result.slashes = relative.slashes;
        result.auth = relative.auth.clone();
        result.host = relative.host.clone();
        result.port = relative.port.clone();
        result.hostname = relative.hostname.clone();
        result.hash = relative.hash.clone();
        result.search = relative.search.clone();
        result.query = relative.query.clone();
        result.pathname = relative.pathname.clone();
        result.path = relative.path.clone();
        result.href = relative.href.clone();

        if is_slashed_protocol(result.protocol.as_deref())
            && result.hostname.as_deref().is_some_and(|h| !h.is_empty())
            && result.pathname.is_none()
        {
            result.path = Some("/".to_string());
            result.pathname = Some("/".to_string());
        }

        result.href = legacy_format_state(&result);
        return result.href.clone();
    }

    if relative.protocol.is_some() && relative.protocol != result.protocol {
        if !is_slashed_protocol(relative.protocol.as_deref()) {
            relative.href = legacy_format_state(&relative);
            return relative.href;
        }

        result.protocol = relative.protocol.clone();

        if relative.host.is_none()
            && !is_file_protocol(relative.protocol.as_deref())
            && !is_hostless_protocol(relative.protocol.as_deref())
        {
            let mut rel_path = split_path(relative.pathname.as_deref());
            let mut new_host = String::new();
            while !rel_path.is_empty() {
                let segment = rel_path.remove(0);
                if !segment.is_empty() {
                    new_host = segment;
                    break;
                }
            }
            relative.host = Some(new_host.clone());
            if relative.hostname.is_none() {
                relative.hostname = Some(new_host);
            }
            if rel_path.first().map(|s| s.as_str()) != Some("") {
                rel_path.insert(0, String::new());
            }
            if rel_path.len() < 2 {
                rel_path.insert(0, String::new());
            }
            result.pathname = Some(rel_path.join("/"));
        } else {
            result.pathname = relative.pathname.clone();
        }

        result.search = relative.search.clone();
        result.query = relative.query.clone();
        result.host = Some(relative.host.clone().unwrap_or_default());
        result.auth = relative.auth.clone();
        result.hostname = Some(
            relative
                .hostname
                .clone()
                .unwrap_or_else(|| relative.host.clone().unwrap_or_default()),
        );
        result.port = relative.port.clone();

        if result.pathname.is_some() || result.search.is_some() {
            result.path = Some(format!(
                "{}{}",
                result.pathname.clone().unwrap_or_default(),
                result.search.clone().unwrap_or_default()
            ));
        }

        result.slashes = result.slashes || relative.slashes;
        result.href = legacy_format_state(&result);
        return result.href.clone();
    }

    if relative.protocol == result.protocol && is_slashed_protocol(result.protocol.as_deref()) {
        relative.protocol = None;
        if relative.host.is_some()
            && relative.pathname.is_none()
            && relative.search.is_none()
            && relative.hash.is_none()
        {
            relative.pathname = relative.host.take();
            relative.hostname = None;
            relative.port = None;
        }
    }

    let is_source_abs = result
        .pathname
        .as_deref()
        .is_some_and(|path| path.starts_with('/'));
    let is_rel_abs = relative.host.is_some()
        || relative
            .pathname
            .as_deref()
            .is_some_and(|path| path.starts_with('/'));
    let mut must_end_abs =
        is_rel_abs || is_source_abs || (result.host.is_some() && relative.pathname.is_some());
    let remove_all_dots = must_end_abs;
    let mut src_path = split_path(result.pathname.as_deref());
    let mut rel_path = split_path(relative.pathname.as_deref());
    let no_leading_slashes =
        result.protocol.is_some() && !is_slashed_protocol(result.protocol.as_deref());

    if no_leading_slashes {
        result.hostname = None;
        result.port = None;
        if let Some(host) = result.host.take() {
            if src_path.first().map(|s| s.as_str()) == Some("") {
                src_path[0] = host;
            } else {
                src_path.insert(0, host);
            }
        }
        if relative.protocol.is_some() {
            relative.hostname = None;
            relative.port = None;
            result.auth = None;
            if let Some(host) = relative.host.take() {
                if rel_path.first().map(|s| s.as_str()) == Some("") {
                    rel_path[0] = host;
                } else {
                    rel_path.insert(0, host);
                }
            }
        }
        must_end_abs = must_end_abs
            && (rel_path.first().map(|s| s.as_str()) == Some("")
                || src_path.first().map(|s| s.as_str()) == Some(""));
    }

    if is_rel_abs {
        if let Some(relative_host) = relative.host.clone() {
            if result.host != Some(relative_host.clone()) {
                result.auth = None;
            }
            result.host = Some(relative_host);
            result.port = relative.port.clone();
        }
        if let Some(relative_hostname) = relative.hostname.clone() {
            if result.hostname != Some(relative_hostname.clone()) {
                result.auth = None;
            }
            result.hostname = Some(relative_hostname);
        }
        result.search = relative.search.clone();
        result.query = relative.query.clone();
        src_path = rel_path;
    } else if !rel_path.is_empty() {
        if !src_path.is_empty() {
            src_path.pop();
        }
        src_path.extend(rel_path);
        result.search = relative.search.clone();
        result.query = relative.query.clone();
    } else if relative.search.is_some() {
        if no_leading_slashes {
            result.hostname = src_path.first().cloned();
            result.host = result.hostname.clone();
            if !src_path.is_empty() {
                src_path.remove(0);
            }
            split_auth_from_host(&mut result);
        }
        result.search = relative.search.clone();
        result.query = relative.query.clone();
        if result.pathname.is_some() || result.search.is_some() {
            result.path = Some(format!(
                "{}{}",
                result.pathname.clone().unwrap_or_default(),
                result.search.clone().unwrap_or_default()
            ));
        }
        result.href = legacy_format_state(&result);
        return result.href.clone();
    }

    if src_path.is_empty() {
        result.pathname = None;
        result.path = result.search.as_ref().map(|search| format!("/{search}"));
        result.href = legacy_format_state(&result);
        return result.href.clone();
    }

    let mut last = src_path.last().cloned().unwrap_or_default();
    let has_trailing_slash = (((result.host.is_some() || relative.host.is_some())
        || src_path.len() > 1)
        && (last == "." || last == ".."))
        || last.is_empty();

    let mut up = 0usize;
    for i in (0..src_path.len()).rev() {
        last = src_path[i].clone();
        if last == "." {
            src_path.remove(i);
        } else if last == ".." {
            src_path.remove(i);
            up += 1;
        } else if up > 0 {
            src_path.remove(i);
            up -= 1;
        }
    }

    if !must_end_abs && !remove_all_dots {
        for _ in 0..up {
            src_path.insert(0, "..".to_string());
        }
    }

    if must_end_abs
        && src_path.first().map(|s| s.as_str()) != Some("")
        && src_path
            .first()
            .is_none_or(|segment| !segment.starts_with('/'))
    {
        src_path.insert(0, String::new());
    }

    if has_trailing_slash && !src_path.join("/").ends_with('/') {
        src_path.push(String::new());
    }

    let is_absolute = src_path.first().map(|s| s.as_str()) == Some("")
        || src_path
            .first()
            .is_some_and(|segment| segment.starts_with('/'));

    if no_leading_slashes {
        result.hostname = if is_absolute {
            Some(String::new())
        } else if !src_path.is_empty() {
            Some(src_path.remove(0))
        } else {
            Some(String::new())
        };
        result.host = result.hostname.clone();
        split_auth_from_host(&mut result);
    }

    must_end_abs = must_end_abs || (result.host.is_some() && !src_path.is_empty());
    if must_end_abs && !is_absolute {
        src_path.insert(0, String::new());
    }

    if src_path.is_empty() {
        result.pathname = None;
        result.path = None;
    } else {
        result.pathname = Some(src_path.join("/"));
    }

    if result.pathname.is_some() || result.search.is_some() {
        result.path = Some(format!(
            "{}{}",
            result.pathname.clone().unwrap_or_default(),
            result.search.clone().unwrap_or_default()
        ));
    }

    if relative.auth.is_some() {
        result.auth = relative.auth.clone();
    }
    result.slashes = result.slashes || relative.slashes;
    result.href = legacy_format_state(&result);
    result.href.clone()
}

fn parse_legacy_resolve_input(input: &str) -> Option<LegacyUrlState> {
    if let Some(query) = input.strip_prefix('?') {
        return Some(LegacyUrlState {
            search: Some(format!("?{query}")),
            query: Some(query.to_string()),
            href: input.to_string(),
            ..Default::default()
        });
    }

    if let Some(hash) = input.strip_prefix('#') {
        return Some(LegacyUrlState {
            hash: Some(format!("#{hash}")),
            href: input.to_string(),
            ..Default::default()
        });
    }

    let rest = input.strip_prefix("//")?;
    let (before_hash, hash) = split_once_or_self(rest, '#');
    let (before_query, query) = split_once_or_self(before_hash, '?');
    let (authority, pathname) = split_once_or_self(before_query, '/');

    let mut state = LegacyUrlState {
        slashes: true,
        hash: hash.map(|fragment| format!("#{fragment}")),
        search: query.map(|value| format!("?{value}")),
        query: query.map(str::to_string),
        href: input.to_string(),
        ..Default::default()
    };

    if let Some((auth, host)) = authority.rsplit_once('@') {
        if !auth.is_empty() {
            state.auth = Some(auth.to_string());
        }
        if !host.is_empty() {
            state.host = Some(host.to_string());
            state.hostname = Some(host.to_string());
        }
    } else if !authority.is_empty() {
        state.host = Some(authority.to_string());
        state.hostname = Some(authority.to_string());
    }

    if let Some(pathname) = pathname {
        state.pathname = Some(format!("/{}", pathname));
    }

    if state.pathname.is_some() || state.search.is_some() {
        state.path = Some(format!(
            "{}{}",
            state.pathname.clone().unwrap_or_default(),
            state.search.clone().unwrap_or_default()
        ));
    }

    Some(state)
}

fn preserve_file_slash_style(source: &str, resolved: &str) -> String {
    if source.starts_with("file:/") && !source.starts_with("file:///") {
        return resolved.replacen("file:///", "file:/", 1);
    }
    resolved.to_string()
}

fn preserve_file_slash_style_for_state(source: &LegacyUrlState, resolved: &str) -> String {
    if is_file_protocol(source.protocol.as_deref()) && !source.slashes && source.host.is_none() {
        return resolved.replacen("file:///", "file:/", 1);
    }
    resolved.to_string()
}

fn split_auth_from_host(state: &mut LegacyUrlState) {
    if let Some(host) = state.host.clone()
        && let Some(index) = host.rfind('@')
        && index > 0
    {
        state.auth = Some(host[..index].to_string());
        let host_only = host[index + 1..].to_string();
        state.host = Some(host_only.clone());
        state.hostname = Some(host_only);
    }
}

fn split_path(pathname: Option<&str>) -> Vec<String> {
    pathname
        .filter(|path| !path.is_empty())
        .map(|path| path.split('/').map(|segment| segment.to_string()).collect())
        .unwrap_or_default()
}

fn legacy_format_state(state: &LegacyUrlState) -> String {
    let mut auth = state.auth.clone().unwrap_or_default();
    if !auth.is_empty() {
        auth = encode_legacy_auth(&auth);
        auth.push('@');
    }

    let mut protocol = state.protocol.clone().unwrap_or_default();
    let mut pathname = state.pathname.clone().unwrap_or_default();
    let mut hash = state.hash.clone().unwrap_or_default();
    let mut host = String::new();

    if let Some(existing_host) = state.host.clone() {
        host = format!("{auth}{existing_host}");
    } else if let Some(hostname) = state.hostname.clone() {
        if !hostname.is_empty() {
            let rendered =
                if hostname.contains(':') && !hostname.starts_with('[') && !hostname.ends_with(']')
                {
                    format!("[{hostname}]")
                } else {
                    hostname
                };
            host = format!("{auth}{rendered}");
            if let Some(port) = state.port.clone()
                && !port.is_empty()
            {
                host.push(':');
                host.push_str(&port);
            }
        }
    }

    let mut search = state.search.clone().unwrap_or_default();

    if !protocol.is_empty() && !protocol.ends_with(':') {
        protocol.push(':');
    }

    let mut rewritten_pathname = String::new();
    let mut last_pos = 0usize;
    for (idx, ch) in pathname.char_indices() {
        let replacement = match ch {
            '#' => Some("%23"),
            '?' => Some("%3F"),
            _ => None,
        };
        if let Some(replacement) = replacement {
            if idx > last_pos {
                rewritten_pathname.push_str(&pathname[last_pos..idx]);
            }
            rewritten_pathname.push_str(replacement);
            last_pos = idx + ch.len_utf8();
        }
    }
    if last_pos > 0 {
        if last_pos != pathname.len() {
            pathname = rewritten_pathname + &pathname[last_pos..];
        } else {
            pathname = rewritten_pathname;
        }
    }

    if state.slashes || is_slashed_protocol(Some(protocol.as_str())) {
        if state.slashes || !host.is_empty() {
            if !pathname.is_empty() && !pathname.starts_with('/') {
                pathname.insert(0, '/');
            }
            host = format!("//{host}");
        }
    }

    search = search.replace('#', "%23");

    if !hash.is_empty() && !hash.starts_with('#') {
        hash.insert(0, '#');
    }
    if !search.is_empty() && !search.starts_with('?') {
        search.insert(0, '?');
    }

    format!("{protocol}{host}{pathname}{search}{hash}")
}

fn encode_legacy_auth(input: &str) -> String {
    let mut out = String::new();
    for byte in input.as_bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(
                *byte,
                b'!' | b'-' | b'.' | b'_' | b'~' | b'\'' | b'(' | b')' | b'*' | b':'
            );
        if keep {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

fn is_slashed_protocol(protocol: Option<&str>) -> bool {
    matches!(
        protocol.unwrap_or_default(),
        "http:" | "https:" | "ftp:" | "gopher:" | "file:" | "ws:" | "wss:"
    )
}

fn is_hostless_protocol(protocol: Option<&str>) -> bool {
    matches!(protocol.unwrap_or_default(), "javascript:")
}

fn is_file_protocol(protocol: Option<&str>) -> bool {
    protocol.unwrap_or_default().eq_ignore_ascii_case("file:")
}

fn parse_windows_option(value: Option<&Value>) -> Result<Option<bool>, VmError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }

    let Some(obj) = value.as_object() else {
        return Err(VmError::type_error("options must be an object"));
    };

    Ok(obj
        .get(&PropertyKey::string("windows"))
        .map(|v| v.to_boolean()))
}

fn primitive_string_arg(value: Option<&Value>, name: &str) -> Result<String, VmError> {
    value
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .ok_or_else(|| VmError::type_error(format!("The \"{name}\" argument must be a string")))
}

fn resolve_base_arg(value: Option<&Value>) -> Result<Option<String>, VmError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_undefined() {
        return Ok(None);
    }
    if let Some(s) = value.as_string() {
        return Ok(Some(s.as_str().to_string()));
    }
    if let Some(obj) = value.as_object()
        && is_branded_url_object(&obj)
    {
        return Ok(get_string_prop(&obj, HREF_KEY));
    }
    Err(VmError::type_error(
        "The \"base\" argument must be a string or URL",
    ))
}

fn parse_url_input(input: &str, base: Option<&str>) -> Result<Url, VmError> {
    if let Some(base) = base {
        let base_url = Url::parse(base).map_err(|e| VmError::type_error(e.to_string()))?;
        base_url
            .join(input)
            .map_err(|e| VmError::type_error(e.to_string()))
    } else {
        Url::parse(input).map_err(|e| VmError::type_error(e.to_string()))
    }
}

fn build_url_value(ncx: &mut NativeContext, parsed: &Url) -> Result<Value, VmError> {
    let proto = url_prototype(ncx)?;
    let obj = GcRef::new(JsObject::new(Value::object(proto)));
    write_url_state(&obj, parsed);
    Ok(Value::object(obj))
}

fn legacy_url_instance(ncx: &NativeContext) -> Result<GcRef<JsObject>, VmError> {
    let ctor = ncx
        .global()
        .get(&PropertyKey::string(LEGACY_URL_CTOR_KEY))
        .and_then(|v| v.as_object())
        .ok_or_else(|| VmError::type_error("Url constructor is not installed"))?;
    let proto = ctor
        .get(&PropertyKey::string("prototype"))
        .and_then(|v| v.as_object())
        .ok_or_else(|| VmError::type_error("Url.prototype is not available"))?;
    let obj = GcRef::new(JsObject::new(Value::object(proto)));
    init_legacy_url_defaults(&obj);
    Ok(obj)
}

fn url_prototype(ncx: &NativeContext) -> Result<GcRef<JsObject>, VmError> {
    let ctor = ncx
        .global()
        .get(&PropertyKey::string("URL"))
        .and_then(|v| v.as_object())
        .ok_or_else(|| VmError::type_error("URL constructor is not installed"))?;
    ctor.get(&PropertyKey::string("prototype"))
        .and_then(|v| v.as_object())
        .ok_or_else(|| VmError::type_error("URL.prototype is not available"))
}

fn write_url_state(obj: &GcRef<JsObject>, parsed: &Url) {
    set_hidden_bool(obj, URL_BRAND_KEY, true);
    set_hidden_string(obj, HREF_KEY, parsed.as_str());
    set_hidden_string(obj, PROTOCOL_KEY, &format!("{}:", parsed.scheme()));
    set_hidden_string(obj, USERNAME_KEY, parsed.username());
    set_hidden_string(obj, PASSWORD_KEY, parsed.password().unwrap_or(""));
    set_hidden_string(obj, HOST_KEY, &host_string(parsed));
    set_hidden_string(obj, HOSTNAME_KEY, &hostname_string(parsed));
    set_hidden_string(
        obj,
        PORT_KEY,
        &parsed.port().map(|p| p.to_string()).unwrap_or_default(),
    );
    set_hidden_string(obj, PATHNAME_KEY, parsed.path());
    set_hidden_string(
        obj,
        SEARCH_KEY,
        parsed
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
            .as_str(),
    );
    set_hidden_string(
        obj,
        HASH_KEY,
        parsed
            .fragment()
            .map(|f| format!("#{f}"))
            .unwrap_or_default()
            .as_str(),
    );
    set_hidden_string(obj, ORIGIN_KEY, &origin_string(parsed));
}

fn host_string(parsed: &Url) -> String {
    match parsed.host() {
        Some(Host::Ipv6(addr)) => match parsed.port() {
            Some(port) => format!("[{addr}]:{port}"),
            None => format!("[{addr}]"),
        },
        Some(host) => match parsed.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        },
        None => String::new(),
    }
}

fn hostname_string(parsed: &Url) -> String {
    match parsed.host() {
        Some(Host::Ipv6(addr)) => addr.to_string(),
        Some(host) => host.to_string(),
        None => String::new(),
    }
}

fn origin_string(parsed: &Url) -> String {
    if parsed.scheme() == "file" {
        return "null".to_string();
    }
    if let Some(host) = parsed.host() {
        let protocol = format!("{}://", parsed.scheme());
        return match (host, parsed.port()) {
            (Host::Ipv6(addr), Some(port)) => format!("{protocol}[{addr}]:{port}"),
            (Host::Ipv6(addr), None) => format!("{protocol}[{addr}]"),
            (host, Some(port)) => format!("{protocol}{host}:{port}"),
            (host, None) => format!("{protocol}{host}"),
        };
    }
    "null".to_string()
}

fn is_branded_url_object(obj: &GcRef<JsObject>) -> bool {
    obj.get(&PropertyKey::string(URL_BRAND_KEY))
        .and_then(|v| v.as_boolean())
        .unwrap_or(false)
}

fn url_prop(this: &Value, key: &str) -> Result<Value, VmError> {
    let obj = this
        .as_object()
        .ok_or_else(|| VmError::type_error("URL method called on non-object"))?;
    if !is_branded_url_object(&obj) {
        return Err(VmError::type_error(
            "URL method called on incompatible receiver",
        ));
    }
    Ok(obj
        .get(&PropertyKey::string(key))
        .unwrap_or(Value::string(JsString::intern(""))))
}

fn get_string_prop(obj: &GcRef<JsObject>, key: &str) -> Option<String> {
    obj.get(&PropertyKey::string(key))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
}

fn value_as_string(value: Option<Value>) -> Option<String> {
    value.and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
}

fn set_plain_prop(obj: &GcRef<JsObject>, key: &str, value: Value) {
    let _ = obj.set(PropertyKey::string(key), value);
}

fn hidden_attrs() -> PropertyAttributes {
    PropertyAttributes {
        writable: true,
        enumerable: false,
        configurable: true,
    }
}

fn set_hidden_string(obj: &GcRef<JsObject>, key: &str, value: &str) {
    obj.define_property(
        PropertyKey::string(key),
        PropertyDescriptor::data_with_attrs(Value::string(JsString::new_gc(value)), hidden_attrs()),
    );
}

fn set_hidden_bool(obj: &GcRef<JsObject>, key: &str, value: bool) {
    obj.define_property(
        PropertyKey::string(key),
        PropertyDescriptor::data_with_attrs(Value::boolean(value), hidden_attrs()),
    );
}

fn set_hidden_value(obj: &GcRef<JsObject>, key: &str, value: Value) {
    obj.define_property(
        PropertyKey::string(key),
        PropertyDescriptor::data_with_attrs(value, hidden_attrs()),
    );
}

fn file_url_input(value: Option<&Value>) -> Result<String, VmError> {
    let Some(value) = value else {
        return Err(VmError::type_error("The \"url\" argument is required"));
    };
    if let Some(s) = value.as_string() {
        return Ok(s.as_str().to_string());
    }
    if let Some(obj) = value.as_object()
        && is_branded_url_object(&obj)
    {
        return Ok(get_string_prop(&obj, HREF_KEY).unwrap_or_default());
    }
    Err(VmError::type_error(
        "The \"url\" argument must be a string or URL",
    ))
}

fn path_to_file_href_posix(path: &str) -> Result<String, VmError> {
    let absolute = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir()
            .map_err(|e| VmError::type_error(e.to_string()))?
            .join(path)
    };

    let normalized = absolute.to_string_lossy().replace('\\', "\\");
    Ok(format!("file://{}", encode_path_with_slashes(&normalized)))
}

fn path_to_file_href_windows(path: &str) -> Result<String, VmError> {
    let path = strip_windows_extended_prefix(path);

    if let Some(rest) = path.strip_prefix("\\\\") {
        let mut parts = rest.split('\\').filter(|s| !s.is_empty());
        let host = parts
            .next()
            .ok_or_else(|| VmError::type_error("Invalid UNC path"))?;
        let share = parts
            .next()
            .ok_or_else(|| VmError::type_error("Invalid UNC path"))?;
        let mut full_path = String::from("/");
        full_path.push_str(share);
        for part in parts {
            full_path.push('/');
            full_path.push_str(part);
        }
        return Ok(format!(
            "file://{}{}",
            host,
            encode_path_with_slashes(&full_path)
        ));
    }

    let normalized = path.replace('\\', "/");
    let absolute = if is_windows_drive_path(&normalized) {
        normalized
    } else {
        let cwd = std::env::current_dir().map_err(|e| VmError::type_error(e.to_string()))?;
        let mut s = cwd.to_string_lossy().replace('/', "\\");
        if !s.ends_with('\\') {
            s.push('\\');
        }
        s.push_str(path);
        s.replace('\\', "/")
    };

    let with_leading_slash = if absolute.starts_with('/') {
        absolute
    } else {
        format!("/{absolute}")
    };
    Ok(format!(
        "file://{}",
        encode_path_with_slashes(&with_leading_slash)
    ))
}

fn strip_windows_extended_prefix(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix("\\\\?\\UNC\\") {
        return Box::leak(format!("\\\\{rest}").into_boxed_str());
    }
    path.strip_prefix("\\\\?\\").unwrap_or(path)
}

fn is_windows_drive_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

fn encode_path_with_slashes(path: &str) -> String {
    let mut out = String::new();
    let leading_slash = path.starts_with('/');
    let trailing_slash = path.ends_with('/');

    for (index, segment) in path.split('/').enumerate() {
        if index > 0 {
            out.push('/');
        }
        if segment.is_empty() {
            continue;
        }
        out.push_str(&utf8_percent_encode(segment, FILE_PATH_ENCODE_SET).to_string());
    }

    if leading_slash && !out.starts_with('/') {
        out.insert(0, '/');
    }
    if trailing_slash && !out.ends_with('/') {
        out.push('/');
    }

    out
}

fn file_url_to_posix_path(parsed: &Url) -> Result<String, VmError> {
    if parsed
        .host_str()
        .is_some_and(|host| !host.is_empty() && host != "localhost")
    {
        return Err(VmError::type_error("File URL host must be empty on POSIX"));
    }
    validate_file_url_path(parsed.path(), false)?;
    percent_decode_str(parsed.path())
        .decode_utf8()
        .map(|s| s.to_string())
        .map_err(|_| VmError::type_error("Invalid file URL path"))
}

fn file_url_to_windows_path(parsed: &Url) -> Result<String, VmError> {
    validate_file_url_path(parsed.path(), true)?;
    let decoded = percent_decode_str(parsed.path())
        .decode_utf8()
        .map_err(|_| VmError::type_error("Invalid file URL path"))?
        .to_string();

    if let Some(host) = parsed.host_str()
        && !host.is_empty()
        && host != "localhost"
    {
        return Ok(format!("\\\\{}{}", host, decoded.replace('/', "\\")));
    }

    let without_leading = decoded.strip_prefix('/').unwrap_or(&decoded);
    Ok(without_leading.replace('/', "\\"))
}

fn validate_file_url_path(path: &str, windows: bool) -> Result<(), VmError> {
    let lower = path.to_ascii_lowercase();
    if lower.contains("%2f") || (windows && lower.contains("%5c")) {
        return Err(VmError::type_error("Invalid file URL path"));
    }
    Ok(())
}

fn init_legacy_url_defaults(obj: &GcRef<JsObject>) {
    set_plain_prop(obj, "protocol", Value::null());
    set_plain_prop(obj, "slashes", Value::null());
    set_plain_prop(obj, "auth", Value::undefined());
    set_plain_prop(obj, "host", Value::null());
    set_plain_prop(obj, "port", Value::null());
    set_plain_prop(obj, "hostname", Value::null());
    set_plain_prop(obj, "hash", Value::null());
    set_plain_prop(obj, "search", Value::null());
    set_plain_prop(obj, "query", Value::null());
    set_plain_prop(obj, "pathname", Value::null());
    set_plain_prop(obj, "path", Value::null());
    set_plain_prop(obj, "href", Value::string(JsString::intern("")));
}

fn populate_legacy_parse(
    obj: &GcRef<JsObject>,
    input: &str,
    parse_query: bool,
    obj_proto: GcRef<JsObject>,
) {
    let trimmed = input.trim_matches(|c: char| c.is_ascii_whitespace());

    if let Some(state) = parse_legacy_special_empty_host(trimmed) {
        write_legacy_state(obj, &state, parse_query, obj_proto);
        return;
    }

    if let Some(state) = parse_legacy_no_slashes(trimmed) {
        write_legacy_state(obj, &state, parse_query, obj_proto);
        return;
    }

    if let Ok(parsed) = Url::parse(trimmed) {
        set_plain_prop(
            obj,
            "protocol",
            Value::string(JsString::new_gc(&format!("{}:", parsed.scheme()))),
        );
        set_plain_prop(obj, "slashes", Value::boolean(true));

        let host = host_string(&parsed);
        let hostname = hostname_string(&parsed);
        set_plain_prop(obj, "host", Value::string(JsString::new_gc(&host)));
        set_plain_prop(obj, "hostname", Value::string(JsString::new_gc(&hostname)));

        if let Some(port) = parsed.port() {
            set_plain_prop(
                obj,
                "port",
                Value::string(JsString::new_gc(&port.to_string())),
            );
        }

        if !parsed.username().is_empty() {
            let auth = if let Some(password) = parsed.password() {
                format!("{}:{password}", parsed.username())
            } else {
                parsed.username().to_string()
            };
            set_plain_prop(obj, "auth", Value::string(JsString::new_gc(&auth)));
        }

        if let Some(fragment) = parsed.fragment() {
            set_plain_prop(
                obj,
                "hash",
                Value::string(JsString::new_gc(&format!("#{fragment}"))),
            );
        }

        if let Some(query) = parsed.query() {
            let search = format!("?{query}");
            set_plain_prop(obj, "search", Value::string(JsString::new_gc(&search)));
        }

        set_plain_prop(
            obj,
            "pathname",
            Value::string(JsString::new_gc(parsed.path())),
        );
        let path = format!(
            "{}{}",
            parsed.path(),
            parsed.query().map(|q| format!("?{q}")).unwrap_or_default()
        );
        set_plain_prop(obj, "path", Value::string(JsString::new_gc(&path)));
        set_plain_prop(
            obj,
            "href",
            Value::string(JsString::new_gc(parsed.as_str())),
        );

        if parse_query {
            set_plain_prop(obj, "query", legacy_query_value(parsed.query(), obj_proto));
        } else if let Some(query) = parsed.query() {
            set_plain_prop(obj, "query", Value::string(JsString::new_gc(query)));
        }

        return;
    }

    let (before_hash, hash) = split_once_or_self(trimmed, '#');
    let (pathname, query) = split_once_or_self(before_hash, '?');

    set_plain_prop(obj, "href", Value::string(JsString::new_gc(trimmed)));
    set_plain_prop(obj, "pathname", Value::string(JsString::new_gc(pathname)));

    let path = if let Some(query) = query {
        format!("{pathname}?{query}")
    } else {
        pathname.to_string()
    };
    set_plain_prop(obj, "path", Value::string(JsString::new_gc(&path)));

    if let Some(query) = query {
        set_plain_prop(
            obj,
            "search",
            Value::string(JsString::new_gc(&format!("?{query}"))),
        );
        if parse_query {
            set_plain_prop(obj, "query", legacy_query_value(Some(query), obj_proto));
        } else {
            set_plain_prop(obj, "query", Value::string(JsString::new_gc(query)));
        }
    } else if parse_query {
        set_plain_prop(obj, "query", legacy_query_value(None, obj_proto));
    }

    if let Some(hash) = hash {
        set_plain_prop(
            obj,
            "hash",
            Value::string(JsString::new_gc(&format!("#{hash}"))),
        );
    }
}

fn parse_legacy_special_empty_host(input: &str) -> Option<LegacyUrlState> {
    let (scheme, rest) = split_scheme(input)?;
    let protocol = format!("{}:", scheme.to_ascii_lowercase());
    if !is_slashed_protocol(Some(protocol.as_str())) || !rest.starts_with("///") {
        return None;
    }

    let raw_path = &rest[2..];
    let (before_hash, hash) = split_once_or_self(raw_path, '#');
    let (path, query) = split_once_or_self(before_hash, '?');

    let mut state = LegacyUrlState {
        protocol: Some(protocol),
        slashes: true,
        host: Some(String::new()),
        hostname: Some(String::new()),
        hash: hash.map(|fragment| format!("#{fragment}")),
        search: query.map(|value| format!("?{value}")),
        query: query.map(str::to_string),
        ..Default::default()
    };

    if !path.is_empty() {
        state.pathname = Some(path.to_string());
    }
    if state.pathname.is_some() || state.search.is_some() {
        state.path = Some(format!(
            "{}{}",
            state.pathname.clone().unwrap_or_default(),
            state.search.clone().unwrap_or_default()
        ));
    }
    state.href = legacy_format_state(&state);
    Some(state)
}

fn parse_legacy_no_slashes(input: &str) -> Option<LegacyUrlState> {
    let (scheme, rest) = split_scheme(input)?;
    if rest.starts_with("//") || rest.starts_with("\\\\") {
        return None;
    }

    let protocol = format!("{}:", scheme.to_ascii_lowercase());
    let mut state = LegacyUrlState {
        protocol: Some(protocol),
        ..Default::default()
    };

    if is_slashed_protocol(state.protocol.as_deref()) {
        let (before_hash, hash) = split_once_or_self(rest, '#');
        let (path, query) = split_once_or_self(before_hash, '?');
        if !path.is_empty() {
            state.pathname = Some(path.to_string());
        }
        state.search = query.map(|q| format!("?{q}"));
        state.query = query.map(str::to_string);
        state.hash = hash.map(|fragment| format!("#{fragment}"));
    } else if let Ok(parsed) = Url::parse(input) {
        if !parsed.username().is_empty() {
            state.auth = Some(
                parsed
                    .password()
                    .map(|password| format!("{}:{password}", parsed.username()))
                    .unwrap_or_else(|| parsed.username().to_string()),
            );
        }

        if parsed.host().is_some() {
            state.host = Some(host_string(&parsed));
            state.hostname = Some(hostname_string(&parsed));
            state.port = parsed.port().map(|port| port.to_string());
            if !parsed.path().is_empty() {
                state.pathname = Some(parsed.path().to_string());
            }
        } else {
            populate_legacy_host_and_path(&mut state, parsed.path());
        }

        state.search = parsed.query().map(|query| format!("?{query}"));
        state.query = parsed.query().map(|query| query.to_string());
        state.hash = parsed.fragment().map(|fragment| format!("#{fragment}"));
    } else {
        let (before_hash, hash) = split_once_or_self(rest, '#');
        let (base, query) = split_once_or_self(before_hash, '?');
        populate_legacy_host_and_path(&mut state, base);
        state.search = query.map(|q| format!("?{q}"));
        state.query = query.map(str::to_string);
        state.hash = hash.map(|fragment| format!("#{fragment}"));
    }

    if state.pathname.is_some() || state.search.is_some() {
        state.path = Some(format!(
            "{}{}",
            state.pathname.clone().unwrap_or_default(),
            state.search.clone().unwrap_or_default()
        ));
    }
    state.href = legacy_format_state(&state);
    Some(state)
}

fn split_scheme(input: &str) -> Option<(&str, &str)> {
    let index = input.find(':')?;
    let scheme = &input[..index];
    if scheme.is_empty() {
        return None;
    }

    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.')) {
        return None;
    }

    Some((scheme, &input[index + 1..]))
}

fn populate_legacy_host_and_path(state: &mut LegacyUrlState, base: &str) {
    if base.is_empty() {
        return;
    }

    if base.starts_with('/') {
        state.pathname = Some(base.to_string());
        return;
    }

    if let Some((host, tail)) = base.split_once('/') {
        if !host.is_empty() {
            state.host = Some(host.to_string());
            state.hostname = Some(host.to_string());
        }
        if !tail.is_empty() {
            state.pathname = Some(format!("/{tail}"));
        }
    } else {
        state.host = Some(base.to_string());
        state.hostname = Some(base.to_string());
    }
}

fn write_legacy_state(
    obj: &GcRef<JsObject>,
    state: &LegacyUrlState,
    parse_query: bool,
    obj_proto: GcRef<JsObject>,
) {
    init_legacy_url_defaults(obj);

    if let Some(protocol) = state.protocol.as_ref() {
        set_plain_prop(obj, "protocol", Value::string(JsString::new_gc(protocol)));
    }
    if state.slashes {
        set_plain_prop(obj, "slashes", Value::boolean(true));
    }
    if let Some(auth) = state.auth.as_ref() {
        set_plain_prop(obj, "auth", Value::string(JsString::new_gc(auth)));
    }
    if let Some(host) = state.host.as_ref() {
        set_plain_prop(obj, "host", Value::string(JsString::new_gc(host)));
    }
    if let Some(port) = state.port.as_ref() {
        set_plain_prop(obj, "port", Value::string(JsString::new_gc(port)));
    }
    if let Some(hostname) = state.hostname.as_ref() {
        set_plain_prop(obj, "hostname", Value::string(JsString::new_gc(hostname)));
    }
    if let Some(hash) = state.hash.as_ref() {
        set_plain_prop(obj, "hash", Value::string(JsString::new_gc(hash)));
    }
    if let Some(search) = state.search.as_ref() {
        set_plain_prop(obj, "search", Value::string(JsString::new_gc(search)));
    }
    if parse_query {
        set_plain_prop(
            obj,
            "query",
            legacy_query_value(state.query.as_deref(), obj_proto),
        );
    } else if let Some(query) = state.query.as_ref() {
        set_plain_prop(obj, "query", Value::string(JsString::new_gc(query)));
    }
    if let Some(pathname) = state.pathname.as_ref() {
        set_plain_prop(obj, "pathname", Value::string(JsString::new_gc(pathname)));
    }
    if let Some(path) = state.path.as_ref() {
        set_plain_prop(obj, "path", Value::string(JsString::new_gc(path)));
    }
    set_plain_prop(obj, "href", Value::string(JsString::new_gc(&state.href)));
}

fn legacy_query_value(query: Option<&str>, obj_proto: GcRef<JsObject>) -> Value {
    let obj = GcRef::new(JsObject::new(Value::object(obj_proto)));
    obj.set_prototype(Value::null());
    if let Some(query) = query {
        for pair in query.split('&').filter(|part| !part.is_empty()) {
            let (key, value) = pair
                .split_once('=')
                .map(|(k, v)| (k, v))
                .unwrap_or((pair, ""));
            set_plain_prop(&obj, key, Value::string(JsString::new_gc(value)));
        }
    }
    Value::object(obj)
}

fn split_once_or_self<'a>(input: &'a str, ch: char) -> (&'a str, Option<&'a str>) {
    if let Some((lhs, rhs)) = input.split_once(ch) {
        (lhs, Some(rhs))
    } else {
        (input, None)
    }
}
