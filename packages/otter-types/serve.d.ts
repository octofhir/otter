/**
 * Otter.serve() - Bun-compatible HTTP/HTTPS server API
 */

declare namespace Otter {
  type MaybePromise<T> = T | Promise<T>;

  type ServerWebSocketSendStatus = number;
  type WebSocketReadyState = 0 | 1 | 2 | 3;
  type WebSocketBinaryType = "nodebuffer" | "arraybuffer" | "uint8array";

  type WebSocketCompressor =
    | "disable"
    | "shared"
    | "dedicated"
    | "3KB"
    | "4KB"
    | "8KB"
    | "16KB"
    | "32KB"
    | "64KB"
    | "128KB"
    | "256KB";

  interface ServerWebSocket<T = undefined> {
    send(data: string | BufferSource, compress?: boolean): ServerWebSocketSendStatus;
    sendText(data: string, compress?: boolean): ServerWebSocketSendStatus;
    sendBinary(data: BufferSource, compress?: boolean): ServerWebSocketSendStatus;
    close(code?: number, reason?: string): void;
    terminate(): void;
    ping(data?: string | BufferSource): ServerWebSocketSendStatus;
    pong(data?: string | BufferSource): ServerWebSocketSendStatus;

    publish(topic: string, data: string | BufferSource, compress?: boolean): ServerWebSocketSendStatus;
    publishText(topic: string, data: string, compress?: boolean): ServerWebSocketSendStatus;
    publishBinary(topic: string, data: BufferSource, compress?: boolean): ServerWebSocketSendStatus;

    subscribe(topic: string): void;
    unsubscribe(topic: string): void;
    isSubscribed(topic: string): boolean;
    readonly subscriptions: string[];

    cork<U = unknown>(callback: (ws: ServerWebSocket<T>) => U): U;

    readonly remoteAddress: string;
    data: T;
    readonly readyState: WebSocketReadyState;
    binaryType: WebSocketBinaryType;
    getBufferedAmount(): number;
  }

  interface WebSocketHandler<T = undefined> {
    data?: T;
    message(ws: ServerWebSocket<T>, message: string | Buffer): void | Promise<void>;
    open?(ws: ServerWebSocket<T>): void | Promise<void>;
    drain?(ws: ServerWebSocket<T>): void | Promise<void>;
    close?(ws: ServerWebSocket<T>, code: number, reason: string): void | Promise<void>;
    ping?(ws: ServerWebSocket<T>, data: Buffer): void | Promise<void>;
    pong?(ws: ServerWebSocket<T>, data: Buffer): void | Promise<void>;

    maxPayloadLength?: number;
    backpressureLimit?: number;
    closeOnBackpressureLimit?: boolean;
    idleTimeout?: number;
    publishToSelf?: boolean;
    sendPings?: boolean;
    perMessageDeflate?:
      | boolean
      | {
          compress?: WebSocketCompressor | boolean;
          decompress?: WebSocketCompressor | boolean;
        };
  }

  type ErrorLike = Error | string;

  interface TLSOptions {
    passphrase?: string;
    dhParamsFile?: string;
    serverName?: string;
    lowMemoryMode?: boolean;
    rejectUnauthorized?: boolean;
    requestCert?: boolean;

    ca?: string | BufferSource | Array<string | BufferSource> | undefined;
    cert?: string | BufferSource | Array<string | BufferSource> | undefined;
    key?: string | BufferSource | Array<string | BufferSource> | undefined;

    secureOptions?: number | undefined;
    ALPNProtocols?: string | BufferSource;
    ciphers?: string;
    clientRenegotiationLimit?: number;
    clientRenegotiationWindow?: number;
  }

  type CookieMap = Record<string, string>;

  interface OtterRequest<T extends string = string> extends Request {
    readonly params: Record<string, string>;
    readonly cookies: CookieMap;
    clone(): OtterRequest<T>;
  }

  type RouteHandler<R extends string = string, WebSocketData = undefined> = (
    req: OtterRequest<R>,
    server: Server<WebSocketData>
  ) => MaybePromise<Response | void | undefined>;

  type RouteValue<R extends string = string, WebSocketData = undefined> =
    | Response
    | RouteHandler<R, WebSocketData>
    | {
        [method: string]: RouteHandler<R, WebSocketData> | Response | false;
      }
    | false;

  type Routes<WebSocketData = undefined> = Record<string, RouteValue<string, WebSocketData>>;

  type Development =
    | boolean
    | {
        hmr?: boolean;
        console?: boolean;
      };

  interface ServeOptions<WebSocketData = undefined> {
    hostname?: "0.0.0.0" | "127.0.0.1" | "localhost" | (string & {});
    port?: string | number;
    unix?: string;
    reusePort?: boolean;
    ipv6Only?: boolean;
    idleTimeout?: number;

    tls?: TLSOptions | TLSOptions[];
    maxRequestBodySize?: number;
    development?: Development;
    error?: (this: Server<WebSocketData>, error: ErrorLike) => MaybePromise<Response | void>;
    id?: string | null;

    fetch?: (
      this: Server<WebSocketData>,
      req: Request,
      server: Server<WebSocketData>
    ) => MaybePromise<Response | void | undefined>;

    routes?: Routes<WebSocketData>;
    websocket?: WebSocketHandler<WebSocketData>;

    // Otter-specific extensions
    http2?: boolean;
    h2c?: boolean;
  }

  interface SocketAddress {
    address: string;
    port: number;
    family: "IPv4" | "IPv6";
  }

  interface Server<WebSocketData = undefined> {
    stop(closeActiveConnections?: boolean): Promise<void>;
    reload<R extends string>(options: ServeOptions<WebSocketData>): Server<WebSocketData>;
    fetch(request: Request | string): Response | Promise<Response>;

    upgrade(
      request: Request,
      options?: {
        headers?: HeadersInit;
        data?: WebSocketData;
      }
    ): boolean;

    publish(topic: string, data: string | ArrayBufferView | ArrayBuffer | SharedArrayBuffer, compress?: boolean): ServerWebSocketSendStatus;
    subscriberCount(topic: string): number;
    requestIP(request: Request): SocketAddress | null;
    timeout(request: Request, seconds: number): void;
    ref(): void;
    unref(): void;

    readonly pendingRequests: number;
    readonly pendingWebSockets: number;
    readonly url: URL;
    readonly port: number | undefined;
    readonly hostname: string | undefined;
    readonly protocol: "http" | "https" | null;
    readonly development: boolean;
    readonly id: string;
  }

  function serve<WebSocketData = undefined>(options: ServeOptions<WebSocketData>): Server<WebSocketData>;

  const Server: {
    prototype: Server;
  };
}
