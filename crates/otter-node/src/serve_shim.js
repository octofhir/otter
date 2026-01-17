/**
 * Otter.serve() JavaScript shim
 *
 * High-performance HTTP server .
 * Supports HTTP/1.1, HTTP/2, and HTTPS.
 */
(function (global) {
  "use strict";

  // Map of server ID -> server options (fetch handler, error handler)
  const servers = new Map();

  // Helper to convert Uint8Array to base64 using built-in btoa
  function uint8ToBase64(bytes) {
    if (bytes.length === 0) return "";
    let binaryString = "";
    for (let i = 0; i < bytes.length; i++) {
      binaryString += String.fromCharCode(bytes[i]);
    }
    return btoa(binaryString);
  }

  /**
   * Called by Rust when an HTTP request arrives.
   * This is invoked directly via JSObjectCallAsFunction for minimal overhead.
   *
   * @param {number} serverId - Server instance ID
   * @param {number} requestId - Request ID in the thread-local store
   */
  global.__otter_http_dispatch = function (serverId, requestId) {
    const server = servers.get(serverId);
    if (!server) {
      // Server was stopped, send 503 Service Unavailable
      __otter_http_respond(requestId, 503, {}, "Service Unavailable");
      return;
    }

    // Create Request wrapper with eager metadata loading
    const request = new OtterRequest(requestId);

    // Handle the request - avoid Promise.resolve() overhead for sync handlers
    try {
      const result = server.fetch(request);

      // Check if result is a Promise (async handler)
      if (result && typeof result.then === "function") {
        // Async handler - use promise chain
        result
          .then((response) => sendResponse(requestId, response))
          .catch((error) => handleError(requestId, server, error));
      } else {
        // Sync handler - send response immediately
        sendResponse(requestId, result);
      }
    } catch (error) {
      // Sync handler threw an error
      handleError(requestId, server, error);
    }
  };

  /**
   * Send response back to Rust.
   * @param {number} requestId
   * @param {Response|string|object} response
   */
  async function sendResponse(requestId, response) {
    try {
      // Fast path: plain string response
      if (typeof response === "string") {
        __otter_http_respond_text(requestId, 200, response);
        return;
      }

      // Fast path: plain object -> JSON
      if (response && typeof response === "object" && !(response instanceof Response)) {
        const jsonStr = JSON.stringify(response);
        // Use text path but with JSON content-type
        __otter_http_respond(requestId, 200, { "content-type": "application/json" }, jsonStr);
        return;
      }

      // Handle null/undefined
      if (!response) {
        __otter_http_respond_text(requestId, 200, "OK");
        return;
      }

      // Response object - check for text fast path
      const status = response.status;
      const contentType = response.headers.get("content-type") || "";

      // Fast path: text-based responses without custom headers
      // Check if response has minimal headers (just content-type or none)
      let headerCount = 0;
      response.headers.forEach(() => { headerCount++; });

      if (headerCount <= 1 && (contentType === "" || contentType.includes("text/plain"))) {
        // Try to get body as text for fast path
        try {
          const text = await response.text();
          __otter_http_respond_text(requestId, status, text);
          return;
        } catch {
          // Fall through to general path
        }
      }

      // General path: extract headers and body
      const headers = {};
      response.headers.forEach((value, key) => {
        headers[key] = value;
      });

      // Get body as bytes
      let bodyBytes;
      try {
        const buffer = await response.arrayBuffer();
        bodyBytes = new Uint8Array(buffer);
      } catch {
        bodyBytes = new Uint8Array(0);
      }

      // Use base64 encoding for binary data transfer
      const bodyBase64 = uint8ToBase64(bodyBytes);

      // Send to Rust with base64-encoded body
      __otter_http_respond(requestId, status, headers, { type: "base64", data: bodyBase64 });
    } catch (error) {
      console.error("Failed to send response:", error);
      __otter_http_respond_text(requestId, 500, "Internal Server Error");
    }
  }

  /**
   * Handle errors from the fetch handler.
   * @param {number} requestId
   * @param {object} server
   * @param {Error} error
   */
  function handleError(requestId, server, error) {
    // Log error with stack trace if available
    const errorMsg = error?.message || error?.toString?.() || String(error);
    const errorStack = error?.stack || "(no stack)";
    console.error(`Request handler error: ${errorMsg}`);
    console.error(`Stack: ${errorStack}`);

    // Try the error handler if provided
    if (server.error) {
      try {
        const errorResponse = server.error(error);
        if (errorResponse instanceof Response) {
          sendResponse(requestId, errorResponse);
          return;
        }
      } catch (e) {
        console.error("Error handler failed:", e);
      }
    }

    // Default error response
    __otter_http_respond(
      requestId,
      500,
      { "Content-Type": "text/plain" },
      "Internal Server Error"
    );
  }

  /**
   * Native Request wrapper with lazy headers loading.
   * Fetches basic metadata (method + full url) upfront, headers only when accessed.
   * This is faster when handlers don't need headers (common case).
   */
  class OtterRequest {
    #id;
    #method;    // Fetched upfront
    #url;       // Full URL (fetched upfront from Rust)
    #headers;   // Headers object (lazy)

    constructor(requestId) {
      this.#id = requestId;
      // Fast path: fetch method + full URL (Rust constructs full URL)
      const basic = __otter_http_req_basic(requestId);
      this.#method = basic?.method || "GET";
      this.#url = basic?.url || "http://localhost/";
    }

    get method() {
      return this.#method;
    }

    get url() {
      return this.#url;
    }

    get headers() {
      if (this.#headers === undefined) {
        // Lazy load headers only when accessed
        const hdrs = __otter_http_req_headers(this.#id);
        this.#headers = new Headers(hdrs || {});
      }
      return this.#headers;
    }

    async arrayBuffer() {
      const bytes = await __otter_http_req_body(this.#id);
      if (bytes && bytes.buffer) {
        return bytes.buffer;
      }
      return new ArrayBuffer(0);
    }

    async text() {
      const buffer = await this.arrayBuffer();
      return new TextDecoder().decode(buffer);
    }

    async json() {
      const text = await this.text();
      return JSON.parse(text);
    }

    async blob() {
      const buffer = await this.arrayBuffer();
      return new Blob([buffer]);
    }

    async formData() {
      // Basic form data parsing
      const text = await this.text();
      const formData = new FormData();
      const contentType = this.headers.get("content-type") || "";

      if (contentType.includes("application/x-www-form-urlencoded")) {
        const params = new URLSearchParams(text);
        for (const [key, value] of params) {
          formData.append(key, value);
        }
      }

      return formData;
    }

    clone() {
      // Note: body can only be read once, so clone returns a new wrapper
      // but body reading will fail if already consumed
      return new OtterRequest(this.#id);
    }
  }

  // Make OtterRequest compatible with standard Request interface
  Object.setPrototypeOf(OtterRequest.prototype, Request.prototype);

  /**
   * Server object returned by Otter.serve()
   */
  class OtterServer {
    #id;
    #port;
    #hostname;
    #stopped = false;

    constructor(id, port, hostname) {
      this.#id = id;
      this.#port = port;
      this.#hostname = hostname;
    }

    get port() {
      return this.#port;
    }

    get hostname() {
      return this.#hostname;
    }

    get url() {
      const protocol = this.#hostname.startsWith("https") ? "https" : "http";
      return `${protocol}://${this.#hostname}:${this.#port}`;
    }

    /**
     * Stop the server gracefully.
     */
    stop() {
      if (this.#stopped) return;
      this.#stopped = true;

      servers.delete(this.#id);
      __otter_http_server_stop(this.#id);
    }

    /**
     * Reload server with new options.
     * @param {object} options - New options to apply
     */
    reload(options) {
      if (this.#stopped) {
        throw new Error("Cannot reload stopped server");
      }

      const current = servers.get(this.#id);
      if (current && options) {
        if (options.fetch) current.fetch = options.fetch;
        if (options.error) current.error = options.error;
      }
    }
  }

  /**
   * Start an HTTP/HTTPS server.
   *
   * @param {object} options - Server configuration
   * @param {number} [options.port=3000] - Port to listen on (0 for random)
   * @param {string} [options.hostname="0.0.0.0"] - Hostname to bind to
   * @param {function} options.fetch - Request handler function
   * @param {function} [options.error] - Error handler function
   * @param {object} [options.tls] - TLS configuration for HTTPS
   * @param {string|Uint8Array} [options.tls.cert] - Certificate PEM
   * @param {string|Uint8Array} [options.tls.key] - Private key PEM
   * @returns {Promise<OtterServer>} Server instance
   */
  async function serve(options) {
    if (!options || typeof options.fetch !== "function") {
      throw new TypeError("Otter.serve requires a fetch handler function");
    }

    const port = options.port ?? 3000;
    const hostname = options.hostname ?? "0.0.0.0";

    // Prepare TLS config if provided
    let tlsConfig = null;
    if (options.tls) {
      const cert =
        options.tls.cert instanceof Uint8Array
          ? options.tls.cert
          : new TextEncoder().encode(options.tls.cert);
      const key =
        options.tls.key instanceof Uint8Array
          ? options.tls.key
          : new TextEncoder().encode(options.tls.key);
      tlsConfig = { cert, key };
    }

    // Create server via native op
    const result = await __otter_http_server_create(port, hostname, tlsConfig);

    if (!result || result.error) {
      throw new Error(result?.error || "Failed to create server");
    }

    const serverId = result.id;
    const actualPort = result.port;

    // Store server options
    servers.set(serverId, {
      fetch: options.fetch,
      error: options.error,
    });

    console.log(
      `HTTP server listening on ${options.tls ? "https" : "http"}://${hostname}:${actualPort}`
    );

    return new OtterServer(serverId, actualPort, hostname);
  }

  // Export Otter.serve
  global.Otter = global.Otter || {};
  global.Otter.serve = serve;

  // Also export Server class for instanceof checks
  global.Otter.Server = OtterServer;
})(globalThis);
