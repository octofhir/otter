/**
 * Otter.serve() - High-performance HTTP/HTTPS server
 *
 * Supports HTTP/1.1 and HTTP/2 with ALPN negotiation for TLS connections.
 */

declare namespace Otter {
	/**
	 * TLS configuration for HTTPS support.
	 */
	interface TlsOptions {
		/**
		 * PEM-encoded certificate chain.
		 */
		cert: string | Uint8Array;

		/**
		 * PEM-encoded private key.
		 */
		key: string | Uint8Array;
	}

	/**
	 * Options for Otter.serve()
	 */
	interface ServeOptions {
		/**
		 * Port to listen on.
		 * Use 0 to get a random available port.
		 * @default 3000
		 */
		port?: number;

		/**
		 * Hostname to bind to.
		 * @default "0.0.0.0"
		 */
		hostname?: string;

		/**
		 * Request handler function.
		 * Called for each incoming HTTP request.
		 *
		 * @param request - The incoming request
		 * @returns A Response object or a Promise that resolves to one
		 */
		fetch(request: Request): Response | Promise<Response>;

		/**
		 * Error handler function.
		 * Called when the fetch handler throws an error.
		 *
		 * @param error - The error that was thrown
		 * @returns A Response object to send, or void to use default 500 response
		 */
		error?(error: Error): Response | void;

		/**
		 * TLS configuration for HTTPS.
		 * If provided, the server will use HTTPS with HTTP/2 support via ALPN.
		 */
		tls?: TlsOptions;
	}

	/**
	 * A running HTTP server instance.
	 */
	interface Server {
		/**
		 * The actual port the server is listening on.
		 * Useful when port 0 was specified to get a random port.
		 */
		readonly port: number;

		/**
		 * The hostname the server is bound to.
		 */
		readonly hostname: string;

		/**
		 * The full URL of the server.
		 */
		readonly url: string;

		/**
		 * Stop the server gracefully.
		 * Existing connections will be allowed to complete.
		 */
		stop(): void;

		/**
		 * Reload the server with new options.
		 * Only fetch and error handlers can be changed.
		 *
		 * @param options - New options to apply
		 */
		reload(options: Partial<Pick<ServeOptions, "fetch" | "error">>): void;
	}

	/**
	 * Start an HTTP/HTTPS server.
	 *
	 * @example
	 * ```typescript
	 * // Basic HTTP server
	 * const server = await Otter.serve({
	 *   port: 3000,
	 *   fetch(req) {
	 *     return new Response("Hello World!");
	 *   }
	 * });
	 *
	 * // HTTPS server with HTTP/2
	 * const httpsServer = await Otter.serve({
	 *   port: 443,
	 *   tls: {
	 *     cert: await Otter.file("cert.pem").text(),
	 *     key: await Otter.file("key.pem").text()
	 *   },
	 *   fetch(req) {
	 *     return new Response("Secure!");
	 *   }
	 * });
	 *
	 * // Random port
	 * const server = await Otter.serve({
	 *   port: 0,
	 *   fetch(req) {
	 *     return new Response("Running on port " + server.port);
	 *   }
	 * });
	 * console.log(`Server running at ${server.url}`);
	 * ```
	 *
	 * @param options - Server configuration
	 * @returns A promise that resolves to a Server instance
	 */
	function serve(options: ServeOptions): Promise<Server>;

	/**
	 * Server class constructor (for instanceof checks).
	 */
	const Server: {
		prototype: Server;
	};
}
