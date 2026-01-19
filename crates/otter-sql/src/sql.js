// Otter SQL - SQL API
// Provides tagged template queries, transactions, and COPY operations.

/**
 * SQL helper functions for tagged templates
 */
function createSqlHelper(value, ...columns) {
    // sql("table_name") - identifier
    if (typeof value === "string" && columns.length === 0) {
        return { __sql_type: "identifier", value };
    }

    // sql(object) or sql(object, ...columns) - for INSERT/UPDATE
    if (typeof value === "object" && value !== null && !Array.isArray(value)) {
        return {
            __sql_type: "object_insert",
            values: value,
            columns: columns.length > 0 ? columns : null,
        };
    }

    // sql([1, 2, 3]) - for IN clause
    if (Array.isArray(value)) {
        // Check if it's an array of objects (bulk insert)
        if (value.length > 0 && typeof value[0] === "object") {
            return {
                __sql_type: "object_insert",
                values: value,
                columns: columns.length > 0 ? columns : null,
            };
        }
        // Simple array for IN clause
        return { __sql_type: "array_in", values: value };
    }

    return value;
}

/**
 * sql.array() - PostgreSQL array literal
 */
createSqlHelper.array = function (values) {
    return { __sql_type: "array_values", values };
};

/**
 * sql.raw() - Raw SQL (use with caution!)
 */
createSqlHelper.raw = function (sql) {
    return { __sql_type: "raw", value: sql };
};

/**
 * SQL class for database connections
 */
class SQL {
    #id = null;
    #adapter = null;
    #url = null;
    #connected = false;
    #pendingConnect = null;

    constructor(urlOrOptions) {
        if (typeof urlOrOptions === "string") {
            this.#url = urlOrOptions;
        } else if (typeof urlOrOptions === "object") {
            this.#url = this.#buildUrl(urlOrOptions);
        } else {
            throw new Error("SQL constructor requires a URL string or options object");
        }
    }

    #buildUrl(options) {
        const adapter = options.adapter || "sqlite";

        if (adapter === "sqlite") {
            return options.database || ":memory:";
        }

        // PostgreSQL
        let url = "postgres://";
        if (options.username) {
            url += encodeURIComponent(options.username);
            if (options.password) {
                url += ":" + encodeURIComponent(options.password);
            }
            url += "@";
        }
        url += options.hostname || "localhost";
        if (options.port) {
            url += ":" + options.port;
        }
        url += "/" + (options.database || "postgres");

        const params = [];
        if (options.ssl) params.push("sslmode=" + options.ssl);
        if (options.max) params.push("max=" + options.max);
        if (params.length > 0) {
            url += "?" + params.join("&");
        }

        return url;
    }

    async #ensureConnected() {
        if (this.#connected) return;

        if (this.#pendingConnect) {
            await this.#pendingConnect;
            return;
        }

        this.#pendingConnect = (async () => {
            const result = await __otter_sql_connect({ url: this.#url });
            this.#id = result.id;
            this.#adapter = result.adapter;
            this.#connected = true;
        })();

        await this.#pendingConnect;
        this.#pendingConnect = null;
    }

    /**
     * Tagged template query function
     */
    async query(strings, ...values) {
        await this.#ensureConnected();

        // Convert tagged template to arrays
        const stringsArray = Array.isArray(strings) ? strings : [strings];

        return __otter_sql_query({
            id: this.#id,
            strings: stringsArray,
            values: values,
            format: "objects",
        });
    }

    /**
     * Execute a statement (returns affected rows)
     */
    async execute(strings, ...values) {
        await this.#ensureConnected();
        const stringsArray = Array.isArray(strings) ? strings : [strings];

        const result = await __otter_sql_execute({
            id: this.#id,
            strings: stringsArray,
            values: values,
        });

        return result.rowsAffected;
    }

    /**
     * Begin a transaction
     */
    async begin(fn) {
        await this.#ensureConnected();

        const tx = new SQLTransaction(this.#id, this.#adapter);
        await tx._begin();

        try {
            const result = await fn(tx);
            await tx._commit();
            return result;
        } catch (error) {
            await tx._rollback();
            throw error;
        }
    }

    /**
     * Reserve a connection for exclusive use
     */
    async reserve() {
        await this.#ensureConnected();
        // For now, return a wrapper around the connection
        // In the future, this could use a dedicated reserved connection
        return new ReservedSQL(this.#id, this.#adapter);
    }

    /**
     * COPY FROM - bulk import (PostgreSQL only)
     */
    async copyFrom(table, options) {
        await this.#ensureConnected();

        if (this.#adapter !== "postgres") {
            throw new Error("COPY FROM is only supported for PostgreSQL");
        }

        let data;
        if (typeof options.source === "string") {
            data = options.source;
        } else if (options.source instanceof Blob) {
            data = await options.source.text();
        } else if (options.source && typeof options.source.text === "function") {
            data = await options.source.text();
        } else {
            throw new Error("source must be a string, Blob, or object with text() method");
        }

        const result = await __otter_sql_copy_from({
            id: this.#id,
            table: table,
            columns: options.columns,
            format: options.format || "text",
            header: options.header || false,
            delimiter: options.delimiter,
            data: data,
        });

        if (options.onCopyEnd) {
            options.onCopyEnd({ rowsCopied: result.rowsCopied });
        }

        return result.rowsCopied;
    }

    /**
     * COPY TO - bulk export (PostgreSQL only)
     * Returns an async iterable of chunks (true streaming!)
     */
    async copyTo(table, options = {}) {
        await this.#ensureConnected();

        if (this.#adapter !== "postgres") {
            throw new Error("COPY TO is only supported for PostgreSQL");
        }

        // Start the streaming COPY TO operation
        const { streamId } = await __otter_sql_copy_to_start({
            id: this.#id,
            table: table,
            columns: options.columns,
            format: options.format || "text",
            header: options.header || false,
            delimiter: options.delimiter,
        });

        // Return a true async iterable that streams chunks
        return {
            async *[Symbol.asyncIterator]() {
                try {
                    while (true) {
                        const result = await __otter_sql_copy_to_read({ streamId });
                        if (result.done) {
                            break;
                        }
                        // Decode base64 chunk
                        const chunk = Uint8Array.from(atob(result.chunk), (c) => c.charCodeAt(0));
                        yield chunk;
                    }
                } finally {
                    // Always close the stream
                    __otter_sql_copy_to_close({ streamId });
                }
            },
        };
    }

    /**
     * COPY TO with query (PostgreSQL only)
     * Returns an async iterable of chunks (true streaming!)
     */
    async copyToQuery(query, options = {}) {
        await this.#ensureConnected();

        if (this.#adapter !== "postgres") {
            throw new Error("COPY TO is only supported for PostgreSQL");
        }

        // Start the streaming COPY TO operation
        const { streamId } = await __otter_sql_copy_to_start({
            id: this.#id,
            query: query,
            columns: options.columns,
            format: options.format || "text",
            header: options.header || false,
            delimiter: options.delimiter,
        });

        // Return a true async iterable that streams chunks
        return {
            async *[Symbol.asyncIterator]() {
                try {
                    while (true) {
                        const result = await __otter_sql_copy_to_read({ streamId });
                        if (result.done) {
                            break;
                        }
                        // Decode base64 chunk
                        const chunk = Uint8Array.from(atob(result.chunk), (c) => c.charCodeAt(0));
                        yield chunk;
                    }
                } finally {
                    // Always close the stream
                    __otter_sql_copy_to_close({ streamId });
                }
            },
        };
    }

    /**
     * Close all connections
     */
    async close(options = {}) {
        if (!this.#connected) return;

        await __otter_sql_close({ id: this.#id });
        this.#connected = false;
        this.#id = null;
    }

    /**
     * Get the adapter type
     */
    get adapter() {
        return this.#adapter;
    }
}

/**
 * Transaction wrapper
 */
class SQLTransaction {
    #id;
    #adapter;
    #active = false;

    constructor(id, adapter) {
        this.#id = id;
        this.#adapter = adapter;
    }

    async _begin() {
        await __otter_sql_begin({ id: this.#id });
        this.#active = true;
    }

    async _commit() {
        if (!this.#active) return;
        await __otter_sql_commit({ id: this.#id });
        this.#active = false;
    }

    async _rollback() {
        if (!this.#active) return;
        await __otter_sql_rollback({ id: this.#id });
        this.#active = false;
    }

    /**
     * Tagged template query within transaction
     */
    async query(strings, ...values) {
        const stringsArray = Array.isArray(strings) ? strings : [strings];
        return __otter_sql_query({
            id: this.#id,
            strings: stringsArray,
            values: values,
            format: "objects",
        });
    }

    /**
     * Create a savepoint
     */
    async savepoint(fn) {
        const name = "sp_" + Math.random().toString(36).slice(2);
        await __otter_sql_savepoint({ id: this.#id, name });

        try {
            const result = await fn(this);
            // Savepoint succeeded, release it
            await __otter_sql_query({
                id: this.#id,
                strings: [`RELEASE SAVEPOINT "${name}"`],
                values: [],
            });
            return result;
        } catch (error) {
            // Rollback to savepoint
            await __otter_sql_query({
                id: this.#id,
                strings: [`ROLLBACK TO SAVEPOINT "${name}"`],
                values: [],
            });
            throw error;
        }
    }
}

/**
 * Reserved connection wrapper
 */
class ReservedSQL {
    #id;
    #adapter;
    #released = false;

    constructor(id, adapter) {
        this.#id = id;
        this.#adapter = adapter;
    }

    async query(strings, ...values) {
        if (this.#released) {
            throw new Error("Connection has been released");
        }
        const stringsArray = Array.isArray(strings) ? strings : [strings];
        return __otter_sql_query({
            id: this.#id,
            strings: stringsArray,
            values: values,
            format: "objects",
        });
    }

    release() {
        this.#released = true;
        // Connection returns to pool automatically
    }
}

/**
 * Create the default sql tagged template function
 */
let defaultSql = null;
let defaultSqlPromise = null;

function createDefaultSql() {
    // Create a tagged template function that also has SQL class methods
    const sqlFn = async function (strings, ...values) {
        if (!defaultSql) {
            if (!defaultSqlPromise) {
                defaultSqlPromise = (async () => {
                    const info = __otter_sql_get_default({});

                    if (info.needsAsyncConnect) {
                        // Need async connect for PostgreSQL
                        const result = await __otter_sql_connect({ url: info.url });
                        defaultSql = { id: result.id, adapter: result.adapter };
                    } else {
                        defaultSql = { id: info.id, adapter: info.adapter };
                    }
                })();
            }
            await defaultSqlPromise;
        }

        const stringsArray = Array.isArray(strings) ? strings : [strings];
        return __otter_sql_query({
            id: defaultSql.id,
            strings: stringsArray,
            values: values,
            format: "objects",
        });
    };

    // Copy helper methods
    Object.assign(sqlFn, createSqlHelper);
    sqlFn.array = createSqlHelper.array;
    sqlFn.raw = createSqlHelper.raw;

    return sqlFn;
}

const sql = createDefaultSql();

// Error classes
class PostgresError extends Error {
    constructor(message, code, detail, hint) {
        super(message);
        this.name = "PostgresError";
        this.code = code;
        this.detail = detail;
        this.hint = hint;
    }
}

class SQLiteError extends Error {
    constructor(message, code) {
        super(message);
        this.name = "SQLiteError";
        this.code = code;
    }
}

// Attach error classes to SQL
SQL.PostgresError = PostgresError;
SQL.SQLiteError = SQLiteError;

// Add to globalThis.Otter (primary namespace)
if (!globalThis.Otter) globalThis.Otter = {};
globalThis.Otter.sql = sql;
globalThis.Otter.SQL = SQL;

// Register the module (additive - don't overwrite existing exports)
if (typeof __registerOtterBuiltin === "function") {
    const existing = (typeof __otter_peek_otter_builtin === "function")
        ? (__otter_peek_otter_builtin("otter") || {})
        : {};
    __registerOtterBuiltin("otter", { ...existing, sql, SQL });
}
