// HTTP Server API - Otter.serve()

const _servers = new Map();
const _sockets = new Map();

function _toHeaderObject(headers) {
    const obj = {};
    if (!headers) return obj;
    if (headers instanceof Headers) {
        headers.forEach((value, key) => {
            obj[key] = value;
        });
        return obj;
    }
    if (Array.isArray(headers)) {
        for (let i = 0; i < headers.length; i++) {
            const pair = headers[i];
            if (!pair || pair.length < 2) continue;
            obj[String(pair[0]).toLowerCase()] = String(pair[1]);
        }
        return obj;
    }
    if (typeof headers === 'object') {
        for (const key of Object.keys(headers)) {
            obj[String(key).toLowerCase()] = String(headers[key]);
        }
    }
    return obj;
}

function _toByteArray(data) {
    if (data == null) return [];
    if (typeof data === 'string') {
        const bytes = new TextEncoder().encode(data);
        return Array.from(bytes);
    }
    if (data instanceof ArrayBuffer) {
        return Array.from(new Uint8Array(data));
    }
    if (ArrayBuffer.isView(data)) {
        return Array.from(new Uint8Array(data.buffer, data.byteOffset, data.byteLength));
    }
    if (typeof SharedArrayBuffer !== 'undefined' && data instanceof SharedArrayBuffer) {
        return Array.from(new Uint8Array(data));
    }
    return Array.from(new TextEncoder().encode(String(data)));
}

function _bytesToBinary(data, binaryType) {
    const bytes = new Uint8Array(data);
    if (binaryType === 'arraybuffer') {
        return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
    }
    if (binaryType === 'nodebuffer' && typeof Buffer !== 'undefined') {
        return Buffer.from(bytes);
    }
    return bytes;
}

function _parseCookies(headerValue) {
    const cookies = {};
    if (!headerValue) return cookies;
    const parts = headerValue.split(';');
    for (let i = 0; i < parts.length; i++) {
        const part = parts[i];
        const idx = part.indexOf('=');
        if (idx === -1) continue;
        const key = part.slice(0, idx).trim();
        const value = part.slice(idx + 1).trim();
        if (!key) continue;
        cookies[key] = value;
    }
    return cookies;
}

class OtterRequest {
    constructor(requestId) {
        const basic = __http_req_basic(requestId);
        this.method = basic.method || 'GET';
        this.url = basic.url || 'http://localhost/';
        this._requestId = requestId;
        this._headersLoaded = false;
        this._headersCache = null;
        this._bodyBytes = null;
        this._parsedUrl = null;
        this._cookies = null;
        this.params = {};

        const defineProperty = Object && typeof Object.defineProperty === 'function'
            ? Object.defineProperty
            : null;

        if (defineProperty) {
            defineProperty(this, 'headers', {
                enumerable: true,
                configurable: true,
                get: () => {
                    if (!this._headersLoaded) {
                        const headers = __http_req_headers(this._requestId);
                        this._headersCache = new Headers(headers);
                        this._headersLoaded = true;
                    }
                    return this._headersCache;
                },
            });
        } else {
            const headers = __http_req_headers(this._requestId);
            this._headersCache = new Headers(headers);
            this._headersLoaded = true;
            this.headers = this._headersCache;
        }
    }

    get parsedUrl() {
        if (this._parsedUrl === null) {
            try {
                this._parsedUrl = new URL(this.url);
            } catch {
                const emptyParams = typeof URLSearchParams === 'function' ? new URLSearchParams() : {
                    get: () => null,
                    has: () => false,
                    toString: () => '',
                };
                this._parsedUrl = { pathname: this.url, search: '', searchParams: emptyParams };
            }
        }
        return this._parsedUrl;
    }

    get pathname() {
        return this.parsedUrl.pathname;
    }

    get search() {
        return this.parsedUrl.search;
    }

    get searchParams() {
        return this.parsedUrl.searchParams;
    }

    get cookies() {
        if (this._cookies === null) {
            const cookieHeader = this.headers.get('cookie');
            this._cookies = _parseCookies(cookieHeader);
        }
        return this._cookies;
    }

    clone() {
        const cloned = new OtterRequest(this._requestId);
        cloned.params = { ...this.params };
        if (this._headersLoaded) {
            cloned._headersLoaded = true;
            cloned._headersCache = new Headers(this._headersCache);
        }
        if (this._bodyBytes) {
            cloned._bodyBytes = new Uint8Array(this._bodyBytes);
        }
        return cloned;
    }

    async _readBodyBytes() {
        if (this._bodyBytes) return this._bodyBytes;
        const bodyArray = await __http_req_body(this._requestId);
        const bytes = new Uint8Array(bodyArray);
        this._bodyBytes = bytes;
        return bytes;
    }

    async arrayBuffer() {
        const bytes = await this._readBodyBytes();
        return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
    }

    async bytes() {
        return await this._readBodyBytes();
    }

    async text() {
        const bytes = await this._readBodyBytes();
        return new TextDecoder().decode(bytes);
    }

    async json() {
        const text = await this.text();
        return JSON.parse(text);
    }

    async blob() {
        const bytes = await this._readBodyBytes();
        return {
            size: bytes.length,
            type: this.headers.get('content-type') || '',
            arrayBuffer: () => Promise.resolve(bytes.buffer),
            text: () => Promise.resolve(new TextDecoder().decode(bytes)),
        };
    }

    async formData() {
        const text = await this.text();
        const contentType = this.headers.get('content-type') || '';

        if (contentType.includes('application/x-www-form-urlencoded')) {
            const fd = new Map();
            const params = new URLSearchParams(text);
            if (typeof params.forEach === 'function') {
                params.forEach((value, key) => {
                    fd.set(key, value);
                });
            }
            return {
                get: (name) => fd.get(name),
                getAll: (name) => (fd.has(name) ? [fd.get(name)] : []),
                has: (name) => fd.has(name),
                entries: () => fd.entries(),
                keys: () => fd.keys(),
                values: () => fd.values(),
            };
        }

        throw new Error('Unsupported content type for formData()');
    }
}

class OtterServerWebSocket {
    constructor(server, socketId, data, remoteAddress) {
        this._server = server;
        this._socketId = socketId;
        this.data = data;
        this.remoteAddress = remoteAddress || '';
        this.binaryType = 'uint8array';
        this._subscriptions = new Set();
        this._readyState = 1;
    }

    get readyState() {
        return this._readyState;
    }

    send(data, compress) {
        const isText = typeof data === 'string';
        const payload = isText ? data : _toByteArray(data);
        return __http_ws_send(this._socketId, payload, isText);
    }

    sendText(data, compress) {
        return __http_ws_send(this._socketId, String(data), true);
    }

    sendBinary(data, compress) {
        const payload = _toByteArray(data);
        return __http_ws_send(this._socketId, payload, false);
    }

    close(code, reason) {
        __http_ws_close(this._socketId, code || 1000, reason || '');
        this._readyState = 2;
    }

    terminate() {
        __http_ws_terminate(this._socketId);
        this._readyState = 3;
    }

    ping(data) {
        const payload = _toByteArray(data || '');
        return __http_ws_ping(this._socketId, payload);
    }

    pong(data) {
        const payload = _toByteArray(data || '');
        return __http_ws_pong(this._socketId, payload);
    }

    publish(topic, data, compress) {
        const isText = typeof data === 'string';
        const payload = isText ? data : _toByteArray(data);
        return __http_ws_publish(this._server._serverId, topic, payload, isText, this._socketId);
    }

    publishText(topic, data, compress) {
        return __http_ws_publish(this._server._serverId, topic, String(data), true, this._socketId);
    }

    publishBinary(topic, data, compress) {
        const payload = _toByteArray(data);
        return __http_ws_publish(this._server._serverId, topic, payload, false, this._socketId);
    }

    subscribe(topic) {
        if (__http_ws_subscribe(this._socketId, topic)) {
            this._subscriptions.add(topic);
        }
    }

    unsubscribe(topic) {
        if (__http_ws_unsubscribe(this._socketId, topic)) {
            this._subscriptions.delete(topic);
        }
    }

    isSubscribed(topic) {
        return this._subscriptions.has(topic);
    }

    get subscriptions() {
        return Array.from(this._subscriptions);
    }

    cork(callback) {
        return callback(this);
    }

    getBufferedAmount() {
        return __http_ws_buffered_amount(this._socketId);
    }
}

function _compileRoutes(routes) {
    const staticRoutes = new Map();
    const paramRoutes = [];
    const wildcardRoutes = [];

    const keys = Object.keys(routes || {});
    for (let i = 0; i < keys.length; i++) {
        const key = keys[i];
        const value = routes[key];
        if (key.includes('*')) {
            const prefix = key.split('*')[0];
            wildcardRoutes.push({ prefix, value, key });
        } else if (key.includes(':')) {
            const segments = key.split('/').filter(Boolean);
            const params = [];
            for (let j = 0; j < segments.length; j++) {
                const segment = segments[j];
                params.push(segment.startsWith(':') ? segment.slice(1) : null);
            }
            paramRoutes.push({ segments, params, value, key });
        } else {
            staticRoutes.set(key, value);
        }
    }

    return { staticRoutes, paramRoutes, wildcardRoutes };
}

function _matchRoute(pathname, routes) {
    const exact = routes.staticRoutes.get(pathname);
    if (exact !== undefined) {
        return { value: exact, params: {} };
    }

    const pathSegments = pathname.split('/').filter(Boolean);
    for (let i = 0; i < routes.paramRoutes.length; i++) {
        const route = routes.paramRoutes[i];
        if (route.segments.length !== pathSegments.length) continue;
        const params = {};
        let matched = true;
        for (let i = 0; i < route.segments.length; i++) {
            const expected = route.segments[i];
            const actual = pathSegments[i];
            if (expected.startsWith(':')) {
                params[expected.slice(1)] = actual;
            } else if (expected !== actual) {
                matched = false;
                break;
            }
        }
        if (matched) {
            return { value: route.value, params };
        }
    }

    for (let i = 0; i < routes.wildcardRoutes.length; i++) {
        const route = routes.wildcardRoutes[i];
        if (pathname.startsWith(route.prefix)) {
            return { value: route.value, params: {} };
        }
    }

    return null;
}

function _resolveRouteValue(routeValue, method) {
    if (routeValue === false) return null;
    if (routeValue instanceof Response) return routeValue;
    if (typeof routeValue === 'function') return routeValue;
    if (routeValue && typeof routeValue === 'object') {
        const handler = routeValue[method] || routeValue[method.toUpperCase()];
        if (handler === false) return null;
        return handler || null;
    }
    return null;
}

class OtterServer {
    constructor(info, options) {
        this._serverId = info.id;
        this.id = options.id || String(info.id);
        this.hostname = info.hostname || undefined;
        this.port = info.port || undefined;
        this.protocol = info.unix ? null : (info.tls ? 'https' : 'http');
        const urlValue = info.unix
            ? 'http://localhost'
            : `${this.protocol}://${this.hostname || 'localhost'}:${this.port || 0}`;
        if (typeof URL === 'function') {
            this.url = new URL(urlValue);
        } else {
            this.url = {
                href: urlValue,
                toString: () => urlValue,
            };
        }
        this._fetchHandler = options.fetch;
        this._errorHandler =
            options.error ||
            ((err) => {
                console.error('Server error:', err);
                return new Response('Internal Server Error', { status: 500 });
            });
        this._websocket = options.websocket || null;
        this._routes = _compileRoutes(options.routes || {});
        _servers.set(info.id, this);
    }

    stop(closeActiveConnections) {
        _servers.delete(this._serverId);
        __http_server_stop(this._serverId);
        return Promise.resolve();
    }

    reload(options) {
        if (options.fetch) this._fetchHandler = options.fetch;
        if (options.error) this._errorHandler = options.error;
        if (options.websocket) this._websocket = options.websocket;
        if (options.routes) this._routes = _compileRoutes(options.routes);
        return this;
    }

    fetch(request) {
        if (typeof this._fetchHandler !== 'function') {
            return new Response('Not Found', { status: 404 });
        }
        return this._fetchHandler(request, this);
    }

    upgrade(request, options) {
        if (!(request instanceof OtterRequest)) return false;
        if (!this._websocket) return false;
        const headers = _toHeaderObject(options && options.headers);
        const data = options && Object.prototype.hasOwnProperty.call(options, 'data') ? options.data : null;
        return __http_ws_upgrade(this._serverId, request._requestId, headers, data);
    }

    publish(topic, data, compress) {
        const isText = typeof data === 'string';
        const payload = isText ? data : _toByteArray(data);
        return __http_ws_publish(this._serverId, topic, payload, isText, null);
    }

    subscriberCount(topic) {
        return __http_ws_subscriber_count(this._serverId, topic);
    }

    requestIP(request) {
        if (!(request instanceof OtterRequest)) return null;
        return __http_req_peer(request._requestId);
    }

    timeout(request, seconds) {
        return;
    }

    ref() {}
    unref() {}

    get pendingRequests() {
        const info = __http_server_pending(this._serverId);
        return info ? info.pendingRequests || 0 : 0;
    }

    get pendingWebSockets() {
        const info = __http_server_pending(this._serverId);
        return info ? info.pendingWebSockets || 0 : 0;
    }

    get development() {
        return false;
    }

}

globalThis.Otter = globalThis.Otter || {};
globalThis.Otter.Server = OtterServer;

globalThis.Otter.serve = async function serve(options) {
    if (typeof options === 'function') {
        options = { fetch: options };
    }

    if (!options || typeof options !== 'object') {
        throw new Error('Otter.serve requires options');
    }

    if (typeof options.fetch !== 'function' && !options.routes) {
        throw new Error('Otter.serve requires a fetch handler function or routes');
    }

    const nativeOptions = {};
    if (options.port !== undefined) nativeOptions.port = options.port;
    if (options.hostname !== undefined) nativeOptions.hostname = options.hostname;
    if (options.unix !== undefined) nativeOptions.unix = options.unix;
    if (options.tls !== undefined) nativeOptions.tls = options.tls;
    if (options.http2 !== undefined) nativeOptions.http2 = options.http2;
    if (options.h2c !== undefined) nativeOptions.h2c = options.h2c;
    if (options.reusePort !== undefined) nativeOptions.reusePort = options.reusePort;
    if (options.ipv6Only !== undefined) nativeOptions.ipv6Only = options.ipv6Only;
    if (options.idleTimeout !== undefined) nativeOptions.idleTimeout = options.idleTimeout;

    if (options.websocket) {
        if (options.websocket === true) {
            nativeOptions.websocket = true;
        } else if (typeof options.websocket === 'object') {
            nativeOptions.websocket = {
                maxPayloadLength: options.websocket.maxPayloadLength,
                backpressureLimit: options.websocket.backpressureLimit,
                closeOnBackpressureLimit: options.websocket.closeOnBackpressureLimit,
                idleTimeout: options.websocket.idleTimeout,
                publishToSelf: options.websocket.publishToSelf,
                sendPings: options.websocket.sendPings,
            };
        }
    }

    const result = await __http_serve(nativeOptions);
    return new OtterServer(result, options);
};

function _sendErrorResponse(requestId, server, err) {
    try {
        const result = server && server._errorHandler ? server._errorHandler(err) : null;
        if (result && typeof result.then === 'function') {
            result
                .then((response) => _sendResponse(requestId, response))
                .catch(() => __http_respond_text(requestId, 500, 'Internal Server Error'));
            return;
        }
        if (result) {
            _sendResponse(requestId, result);
            return;
        }
    } catch {}
    __http_respond_text(requestId, 500, 'Internal Server Error');
}

function _responseBodyToBytes(body) {
    if (body === undefined || body === null) {
        return new Uint8Array(0);
    }

    if (body instanceof Uint8Array) {
        return body;
    }

    if (body instanceof ArrayBuffer) {
        return new Uint8Array(body);
    }

    if (typeof ArrayBuffer !== 'undefined' && typeof ArrayBuffer.isView === 'function' && ArrayBuffer.isView(body)) {
        return new Uint8Array(body.buffer, body.byteOffset, body.byteLength);
    }

    if (Array.isArray(body)) {
        return new Uint8Array(body);
    }

    if (typeof body === 'string') {
        return new TextEncoder().encode(body);
    }

    return new TextEncoder().encode(String(body));
}

function _sendResponse(requestId, response) {
    try {
        if (typeof response === 'string') {
            __http_respond_text(requestId, 200, response);
            return;
        }

        if (response && typeof response === 'object' && !(response instanceof Response)) {
            response = Response.json(response);
        }

        if (!response) {
            __http_respond_text(requestId, 200, 'OK');
            return;
        }

        const status = response.status;
        const headers = _toHeaderObject(response.headers);
        const bodyBytes = _responseBodyToBytes(response._body);

        __http_respond(requestId, status, headers, Array.from(bodyBytes));
    } catch {
        __http_respond_text(requestId, 500, 'Internal Server Error');
    }
}

globalThis.__otter_http_dispatch = function (serverId, requestId) {
    const server = _servers.get(serverId);
    if (!server) {
        __http_respond_text(requestId, 503, 'Service Unavailable');
        return;
    }

    const request = new OtterRequest(requestId);
    const routeMatch = _matchRoute(request.pathname, server._routes);
    if (routeMatch) {
        request.params = routeMatch.params || {};
        const resolved = _resolveRouteValue(routeMatch.value, request.method);
        if (resolved instanceof Response) {
            _sendResponse(requestId, resolved);
            return;
        }
        if (typeof resolved === 'function') {
            try {
                const result = resolved(request, server);
                if (result && typeof result.then === 'function') {
                    result
                        .then((response) => _sendResponse(requestId, response))
                        .catch((err) => _sendErrorResponse(requestId, server, err));
                } else {
                    _sendResponse(requestId, result);
                }
                return;
            } catch (err) {
                _sendErrorResponse(requestId, server, err);
                return;
            }
        }
    }

    try {
        const result = server.fetch(request, server);
        if (result && typeof result.then === 'function') {
            result
                .then((response) => _sendResponse(requestId, response))
                .catch((err) => _sendErrorResponse(requestId, server, err));
        } else {
            _sendResponse(requestId, result);
        }
    } catch (err) {
        _sendErrorResponse(requestId, server, err);
    }
};

globalThis.__otter_ws_dispatch = function (event) {
    if (!event || !event.type) return;
    const server = _servers.get(event.serverId);
    if (!server || !server._websocket) return;

    if (event.type === 'open') {
        const ws = new OtterServerWebSocket(
            server,
            event.socketId,
            event.data,
            event.remoteAddress
        );
        _sockets.set(event.socketId, ws);
        if (server._websocket.data !== undefined && ws.data === undefined) {
            ws.data = server._websocket.data;
        }
        if (typeof server._websocket.open === 'function') {
            Promise.resolve(server._websocket.open(ws)).catch(() => {});
        }
        return;
    }

    const ws = _sockets.get(event.socketId);
    if (!ws) return;

    if (event.type === 'message') {
        const payload = event.binary
            ? _bytesToBinary(event.data, ws.binaryType)
            : event.data;
        if (typeof server._websocket.message === 'function') {
            Promise.resolve(server._websocket.message(ws, payload)).catch(() => {});
        }
        return;
    }

    if (event.type === 'close') {
        ws._readyState = 3;
        _sockets.delete(event.socketId);
        if (typeof server._websocket.close === 'function') {
            Promise.resolve(server._websocket.close(ws, event.code || 1000, event.reason || '')).catch(() => {});
        }
        return;
    }

    if (event.type === 'drain') {
        if (typeof server._websocket.drain === 'function') {
            Promise.resolve(server._websocket.drain(ws)).catch(() => {});
        }
        return;
    }

    if (event.type === 'ping') {
        if (typeof server._websocket.ping === 'function') {
            const payload = _bytesToBinary(event.data || [], 'nodebuffer');
            Promise.resolve(server._websocket.ping(ws, payload)).catch(() => {});
        }
        return;
    }

    if (event.type === 'pong') {
        if (typeof server._websocket.pong === 'function') {
            const payload = _bytesToBinary(event.data || [], 'nodebuffer');
            Promise.resolve(server._websocket.pong(ws, payload)).catch(() => {});
        }
    }
};
