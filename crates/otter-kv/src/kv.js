// Otter KV - Key-value store API
// Provides a simple synchronous key-value store.

/**
 * KVStore class for key-value storage
 */
class KVStore {
    #id = null;
    #path = null;
    #closed = false;

    constructor(path) {
        this.#path = path;
        const result = __otter_kv_open({ path });
        this.#id = result.id;
    }

    /**
     * Set a value for a key
     * @param {string} key - The key
     * @param {any} value - The value (will be JSON serialized)
     */
    set(key, value) {
        if (this.#closed) {
            throw new Error("KVStore is closed");
        }
        __otter_kv_set({ id: this.#id, key, value });
    }

    /**
     * Get a value by key
     * @param {string} key - The key
     * @returns {any} The value, or undefined if not found
     */
    get(key) {
        if (this.#closed) {
            throw new Error("KVStore is closed");
        }
        const result = __otter_kv_get({ id: this.#id, key });
        return result === null ? undefined : result;
    }

    /**
     * Delete a key
     * @param {string} key - The key
     * @returns {boolean} True if the key existed
     */
    delete(key) {
        if (this.#closed) {
            throw new Error("KVStore is closed");
        }
        return __otter_kv_delete({ id: this.#id, key });
    }

    /**
     * Check if a key exists
     * @param {string} key - The key
     * @returns {boolean} True if the key exists
     */
    has(key) {
        if (this.#closed) {
            throw new Error("KVStore is closed");
        }
        return __otter_kv_has({ id: this.#id, key });
    }

    /**
     * Get all keys
     * @returns {string[]} Array of keys
     */
    keys() {
        if (this.#closed) {
            throw new Error("KVStore is closed");
        }
        return __otter_kv_keys({ id: this.#id });
    }

    /**
     * Clear all keys
     */
    clear() {
        if (this.#closed) {
            throw new Error("KVStore is closed");
        }
        __otter_kv_clear({ id: this.#id });
    }

    /**
     * Get the number of keys
     * @returns {number} Number of keys
     */
    get size() {
        if (this.#closed) {
            throw new Error("KVStore is closed");
        }
        return __otter_kv_len({ id: this.#id });
    }

    /**
     * Close the store
     */
    close() {
        if (this.#closed) return;
        __otter_kv_close({ id: this.#id });
        this.#closed = true;
    }

    /**
     * Get the path
     */
    get path() {
        return this.#path;
    }

    /**
     * Check if the store is closed
     */
    get isClosed() {
        return this.#closed;
    }
}

/**
 * Create a KV store
 * @param {string} path - Database path (":memory:" for in-memory, or file path)
 * @returns {KVStore} The KV store instance
 */
function kv(path) {
    return new KVStore(path);
}

// Add to globalThis.Otter (primary namespace)
if (!globalThis.Otter) globalThis.Otter = {};
globalThis.Otter.kv = kv;
globalThis.Otter.KVStore = KVStore;

// Register the module (additive - don't overwrite existing exports like sql, SQL)
if (typeof __registerOtterBuiltin === "function") {
    const existing = (typeof __otter_peek_otter_builtin === "function")
        ? (__otter_peek_otter_builtin("otter") || {})
        : {};
    __registerOtterBuiltin("otter", { ...existing, kv });
}
