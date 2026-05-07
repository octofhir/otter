// Otter Runtime - Global Type Definitions
// Only includes Otter-specific globals, Web APIs come from @types/node

declare global {
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
