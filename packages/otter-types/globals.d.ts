// Otter Runtime - Global Type Definitions
// Only includes Otter-specific globals, Web APIs come from @types/node

declare global {
    type WorkerMessageHandler = (event: WorkerMessageEvent) => void;

    /**
     * One capability class in a worker narrowing request:
     * `false` denies the class, `true` (or omitting it) inherits the
     * parent's rule set, and a pattern array allows only operations
     * matching those patterns — bounded by what the parent also allows,
     * so a request can never escalate.
     */
    type WorkerCapabilityRequest = boolean | string[];

    /** Otter-specific worker options. */
    interface WorkerOtterOptions {
        /**
         * Narrow the worker's capabilities to a subset of the parent's.
         * Each class is evaluated as the intersection of the parent's
         * rule set and the request.
         */
        capabilities?: {
            read?: WorkerCapabilityRequest;
            write?: WorkerCapabilityRequest;
            net?: WorkerCapabilityRequest;
            env?: WorkerCapabilityRequest;
            run?: WorkerCapabilityRequest;
            ffi?: WorkerCapabilityRequest;
        };
    }

    interface WorkerOptions {
        type?: "classic" | "module";
        name?: string;
        credentials?: "omit" | "same-origin" | "include";
        /** Otter extensions (capability narrowing). */
        otter?: WorkerOtterOptions;
    }

    interface WorkerMessageEvent<T = any> {
        readonly type: "message" | "messageerror" | "error";
        readonly data: T;
        readonly message?: string;
    }

    interface WorkerGlobalScope {
        readonly self: WorkerGlobalScope;
        onmessage: WorkerMessageHandler | null;
        onerror: WorkerMessageHandler | null;
        postMessage(value: any, transferList?: any[]): void;
        close(): void;
    }

    class Worker {
        constructor(specifier: string | URL, options?: WorkerOptions);
        onmessage: WorkerMessageHandler | null;
        onerror: WorkerMessageHandler | null;
        onmessageerror: WorkerMessageHandler | null;
        postMessage(value: any, transferList?: any[]): void;
        terminate(): void;
        addEventListener(type: "message" | "messageerror" | "error", listener: WorkerMessageHandler): void;
        removeEventListener(type: "message" | "messageerror" | "error", listener: WorkerMessageHandler): void;
        dispatchEvent(event: WorkerMessageEvent): boolean;
    }

    var self: WorkerGlobalScope & typeof globalThis;

    // ============================================================================
    // CommonJS Support
    // ============================================================================

    /**
     * Require a CommonJS module.
     * @param id Module specifier (path or package name)
     * @returns The module's exports
     */
    function require(id: string): any;

    /**
     * The require function interface with additional properties.
     */
    interface NodeRequire {
        (id: string): any;

        /**
         * Resolve a module path to its absolute path.
         */
        resolve(id: string): string;

        /**
         * Module cache - loaded modules are cached here.
         */
        cache: Record<string, NodeModule>;

        /**
         * The main module (entry point).
         */
        main: NodeModule | undefined;
    }

    /**
     * The module object available in CommonJS modules.
     */
    interface NodeModule {
        /**
         * The module's exports object.
         */
        exports: any;

        /**
         * The require function for this module.
         */
        require: NodeRequire;

        /**
         * The module's unique identifier.
         */
        id: string;

        /**
         * The absolute path to the module file.
         */
        filename: string;

        /**
         * Whether the module has finished loading.
         */
        loaded: boolean;

        /**
         * The module that first required this one.
         */
        parent: NodeModule | null;

        /**
         * Modules that have been required by this module.
         */
        children: NodeModule[];

        /**
         * The search paths for modules.
         */
        paths: string[];
    }

    /**
     * The module object - available in CommonJS modules.
     */
    var module: NodeModule;

    /**
     * Alias to module.exports - available in CommonJS modules.
     */
    var exports: any;

    /**
     * The directory name of the current module - available in CommonJS modules.
     */
    var __dirname: string;

    /**
     * The file name of the current module - available in CommonJS modules.
     */
    var __filename: string;
}

export {};
