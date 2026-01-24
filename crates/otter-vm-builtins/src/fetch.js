// Fetch API implementation
// Web-compatible Headers, Request, Response classes and fetch() function

function _makeIterator(array) {
    let index = 0;
    const iterator = {
        next: () => {
            if (index < array.length) {
                const value = array[index];
                index += 1;
                return { value, done: false };
            }
            return { value: undefined, done: true };
        },
    };
    if (typeof Symbol === 'function' && Symbol.iterator) {
        iterator[Symbol.iterator] = function () { return this; };
    }
    return iterator;
}

class Headers {
    constructor(init) {
        this._headers = {};

        if (init) {
            if (init instanceof Headers) {
                init.forEach((value, key) => this.set(key, value));
            } else if (Array.isArray(init)) {
                for (let i = 0; i < init.length; i++) {
                    const pair = init[i];
                    if (!pair || pair.length < 2) continue;
                    this.append(pair[0], pair[1]);
                }
            } else if (typeof init === 'object') {
                const keys = Object.keys(init);
                for (let i = 0; i < keys.length; i++) {
                    const key = keys[i];
                    this.set(key, init[key]);
                }
            }
        }
    }

    get(name) {
        const key = String(name).toLowerCase();
        const value = this._headers[key];
        return value === undefined ? null : value;
    }

    set(name, value) {
        const key = String(name).toLowerCase();
        this._headers[key] = String(value);
    }

    append(name, value) {
        const key = String(name).toLowerCase();
        const existing = this._headers[key];
        if (existing) {
            this._headers[key] = existing + ', ' + value;
        } else {
            this._headers[key] = String(value);
        }
    }

    delete(name) {
        const key = String(name).toLowerCase();
        delete this._headers[key];
    }

    has(name) {
        const key = String(name).toLowerCase();
        return this._headers[key] !== undefined;
    }

    keys() {
        const keys = Object.keys(this._headers);
        return _makeIterator(keys);
    }

    values() {
        const keys = Object.keys(this._headers);
        const values = new Array(keys.length);
        for (let i = 0; i < keys.length; i++) {
            values[i] = this._headers[keys[i]];
        }
        return _makeIterator(values);
    }

    entries() {
        const keys = Object.keys(this._headers);
        const entries = new Array(keys.length);
        for (let i = 0; i < keys.length; i++) {
            const key = keys[i];
            entries[i] = [key, this._headers[key]];
        }
        return _makeIterator(entries);
    }

    forEach(callback, thisArg) {
        const keys = Object.keys(this._headers);
        for (let i = 0; i < keys.length; i++) {
            const key = keys[i];
            callback(this._headers[key], key, this);
        }
    }

    [Symbol.iterator]() {
        return this.entries();
    }

    toObject() {
        const obj = {};
        const keys = Object.keys(this._headers);
        for (let i = 0; i < keys.length; i++) {
            const key = keys[i];
            obj[key] = this._headers[key];
        }
        return obj;
    }
}

class Request {
    constructor(input, init = {}) {
        if (input instanceof Request) {
            this.url = input.url;
            this.method = input.method;
            this.headers = new Headers(input.headers);
            this._body = input._body;
        } else {
            this.url = String(input);
            this.method = (init.method || 'GET').toUpperCase();
            this.headers = new Headers(init.headers);
            this._body = init.body;
        }

        this.redirect = init.redirect || 'follow';
        this.signal = init.signal || null;
    }

    clone() {
        return new Request(this);
    }

    async text() {
        if (this._body === undefined || this._body === null) return '';
        if (typeof this._body === 'string') return this._body;
        if (this._body instanceof Uint8Array) {
            return new TextDecoder().decode(this._body);
        }
        return String(this._body);
    }

    async json() {
        return JSON.parse(await this.text());
    }

    async arrayBuffer() {
        if (this._body instanceof Uint8Array) {
            return this._body.buffer;
        }
        if (typeof this._body === 'string') {
            return new TextEncoder().encode(this._body).buffer;
        }
        return new ArrayBuffer(0);
    }
}

class Response {
    constructor(body, init = {}) {
        this._body = body;
        this.status = init.status !== undefined ? init.status : 200;
        this.statusText = init.statusText || '';
        this.headers = new Headers(init.headers);
        this.ok = this.status >= 200 && this.status < 300;
        this.redirected = false;
        this.type = 'basic';
        this.url = init.url || '';
    }

    clone() {
        return new Response(this._body, {
            status: this.status,
            statusText: this.statusText,
            headers: this.headers,
            url: this.url
        });
    }

    async text() {
        if (this._body === undefined || this._body === null) return '';
        if (typeof this._body === 'string') return this._body;
        if (this._body instanceof Uint8Array) {
            return new TextDecoder().decode(this._body);
        }
        return String(this._body);
    }

    async json() {
        return JSON.parse(await this.text());
    }

    async arrayBuffer() {
        if (this._body instanceof Uint8Array) {
            return this._body.buffer.slice(
                this._body.byteOffset,
                this._body.byteOffset + this._body.byteLength
            );
        }
        if (typeof this._body === 'string') {
            return new TextEncoder().encode(this._body).buffer;
        }
        return new ArrayBuffer(0);
    }

    async bytes() {
        if (this._body instanceof Uint8Array) {
            return this._body;
        }
        if (typeof this._body === 'string') {
            return new TextEncoder().encode(this._body);
        }
        return new Uint8Array(0);
    }

    async blob() {
        // Basic Blob-like object
        const bytes = await this.bytes();
        return {
            size: bytes.length,
            type: this.headers.get('content-type') || '',
            arrayBuffer: () => Promise.resolve(bytes.buffer),
            text: () => Promise.resolve(new TextDecoder().decode(bytes)),
            slice: (start, end) => bytes.slice(start, end)
        };
    }

    static error() {
        const response = new Response(null, { status: 0, statusText: '' });
        response.type = 'error';
        return response;
    }

    static redirect(url, status = 302) {
        const response = new Response(null, {
            status,
            headers: { Location: url }
        });
        return response;
    }

    static json(data, init = {}) {
        const body = JSON.stringify(data);
        const headers = new Headers(init.headers);
        if (!headers.has('content-type')) {
            headers.set('content-type', 'application/json');
        }
        return new Response(body, {
            ...init,
            headers
        });
    }
}

async function fetch(input, init = {}) {
    const request = input instanceof Request ? input : new Request(input, init);

    // Build headers object for native call
    const headersObj = request.headers.toObject();

    // Prepare body
    let body = null;
    if (request._body !== undefined && request._body !== null) {
        if (typeof request._body === 'string') {
            body = request._body;
        } else if (request._body instanceof Uint8Array) {
            body = Array.from(request._body);
        } else if (request._body instanceof ArrayBuffer) {
            body = Array.from(new Uint8Array(request._body));
        } else {
            body = String(request._body);
        }
    }

    // Call native __fetch
    const result = await __fetch(request.url, request.method, headersObj, body);

    // Build Response from result
    const responseBody = new Uint8Array(result.body);
    return new Response(responseBody, {
        status: result.status,
        statusText: result.statusText,
        headers: result.headers,
        url: request.url
    });
}

// Register globals
globalThis.Headers = Headers;
globalThis.Request = Request;
globalThis.Response = Response;
globalThis.fetch = fetch;
