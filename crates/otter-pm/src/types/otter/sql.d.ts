/**
 * Otter SQL API
 *
 * Provides unified SQLite and PostgreSQL support with tagged template queries,
 * transactions, and COPY operations (PostgreSQL only).
 */

// ============================================================================
// SQL Module
// ============================================================================

declare module "otter" {
	/**
	 * Default SQL tagged template function.
	 * Uses DATABASE_URL environment variable or falls back to SQLite :memory:.
	 *
	 * @example
	 * ```typescript
	 * import { sql } from "otter";
	 *
	 * const users = await sql`SELECT * FROM users`;
	 * const user = await sql`SELECT * FROM users WHERE id = ${userId}`;
	 * ```
	 */
	export const sql: SqlTaggedTemplate & SqlHelpers;

	/**
	 * SQL class for creating database connections.
	 *
	 * @example
	 * ```typescript
	 * import { SQL } from "otter";
	 *
	 * const db = new SQL(":memory:");
	 * const pg = new SQL("postgres://localhost/mydb");
	 * ```
	 */
	export class SQL {
		constructor(url: string);
		constructor(options: SQLOptions);

		/** Execute a tagged template query and return results */
		query<T = any>(
			strings: TemplateStringsArray,
			...values: any[]
		): Promise<T[]>;

		/** Execute a tagged template statement and return affected rows */
		execute(strings: TemplateStringsArray, ...values: any[]): Promise<number>;

		/**
		 * Begin a transaction.
		 *
		 * @example
		 * ```typescript
		 * await db.begin(async (tx) => {
		 *   await tx`INSERT INTO users (name) VALUES (${"Alice"})`;
		 *   await tx`UPDATE accounts SET balance = balance - 100`;
		 *   // auto-commit on success, auto-rollback on error
		 * });
		 * ```
		 */
		begin<T>(fn: (tx: Transaction) => Promise<T>): Promise<T>;

		/**
		 * Reserve a connection for exclusive use.
		 */
		reserve(): Promise<ReservedSQL>;

		/**
		 * COPY FROM - bulk import data (PostgreSQL only).
		 *
		 * @example
		 * ```typescript
		 * await pg.copyFrom("users", {
		 *   columns: ["name", "email"],
		 *   format: "csv",
		 *   source: new Blob(["Alice,alice@example.com\n"]),
		 * });
		 * ```
		 */
		copyFrom(table: string, options: CopyFromOptions): Promise<number>;

		/**
		 * COPY TO - bulk export data (PostgreSQL only).
		 * Returns an async iterable of chunks.
		 *
		 * @example
		 * ```typescript
		 * for await (const chunk of await pg.copyTo("users", { format: "csv" })) {
		 *   process.stdout.write(chunk);
		 * }
		 * ```
		 */
		copyTo(
			table: string,
			options?: CopyToOptions,
		): Promise<AsyncIterable<Uint8Array>>;

		/**
		 * COPY TO with a query (PostgreSQL only).
		 */
		copyToQuery(
			query: string,
			options?: CopyToOptions,
		): Promise<AsyncIterable<Uint8Array>>;

		/**
		 * Close all connections.
		 */
		close(options?: { timeout?: number }): Promise<void>;

		/** Get the adapter type ("sqlite" or "postgres") */
		readonly adapter: "sqlite" | "postgres";

		/** PostgreSQL error class */
		static PostgresError: typeof PostgresError;

		/** SQLite error class */
		static SQLiteError: typeof SQLiteError;
	}

	/**
	 * KV store function.
	 *
	 * @example
	 * ```typescript
	 * import { kv } from "otter";
	 *
	 * const store = kv("./data.kv");
	 * const cache = kv(":memory:");
	 * ```
	 */
	export function kv(path: string): KVStore;

	// ============================================================================
	// SQL Types
	// ============================================================================

	/**
	 * Tagged template function for SQL queries.
	 */
	interface SqlTaggedTemplate {
		<T = any>(strings: TemplateStringsArray, ...values: any[]): Promise<T[]>;
	}

	/**
	 * SQL helper functions.
	 */
	interface SqlHelpers {
		/**
		 * Escape an identifier (table/column name).
		 */
		(value: string): SqlIdentifier;

		/**
		 * Insert object values.
		 */
		(value: object, ...columns: string[]): SqlObjectInsert;

		/**
		 * Insert array of values (for IN clause) or bulk insert.
		 */
		(value: any[]): SqlArrayValues;

		/**
		 * Create a PostgreSQL array literal.
		 */
		array(values: any[]): SqlArray;

		/**
		 * Raw SQL (use with caution!).
		 */
		raw(sql: string): SqlRaw;
	}

	interface SqlIdentifier {
		__sql_type: "identifier";
		value: string;
	}

	interface SqlObjectInsert {
		__sql_type: "object_insert";
		values: object | object[];
		columns: string[] | null;
	}

	interface SqlArrayValues {
		__sql_type: "array_in" | "object_insert";
		values: any[];
		columns?: string[] | null;
	}

	interface SqlArray {
		__sql_type: "array_values";
		values: any[];
	}

	interface SqlRaw {
		__sql_type: "raw";
		value: string;
	}

	/**
	 * SQL connection options.
	 */
	interface SQLOptions {
		/** Database adapter type */
		adapter?: "postgres" | "sqlite";
		/** Hostname for PostgreSQL */
		hostname?: string;
		/** Port for PostgreSQL */
		port?: number;
		/** Database name */
		database?: string;
		/** Username for PostgreSQL */
		username?: string;
		/** Password for PostgreSQL */
		password?: string;
		/** SSL mode for PostgreSQL */
		ssl?: "disable" | "prefer" | "require";
		/** Maximum connections for PostgreSQL pool */
		max?: number;
		/** Idle timeout in seconds */
		idleTimeout?: number;
		/** Connection timeout in seconds */
		connectionTimeout?: number;
	}

	/**
	 * SQL transaction.
	 */
	interface Transaction {
		/** Execute a query within the transaction */
		<T = any>(strings: TemplateStringsArray, ...values: any[]): Promise<T[]>;

		/** Execute a query within the transaction */
		query<T = any>(
			strings: TemplateStringsArray,
			...values: any[]
		): Promise<T[]>;

		/**
		 * Create a savepoint.
		 *
		 * @example
		 * ```typescript
		 * await tx.savepoint(async (sp) => {
		 *   await sp`UPDATE users SET status = 'active'`;
		 *   if (error) throw new Error("rollback to savepoint");
		 * });
		 * ```
		 */
		savepoint<T>(fn: (sp: Transaction) => Promise<T>): Promise<T>;
	}

	/**
	 * Reserved SQL connection.
	 */
	interface ReservedSQL {
		/** Execute a query on the reserved connection */
		query<T = any>(
			strings: TemplateStringsArray,
			...values: any[]
		): Promise<T[]>;

		/** Release the connection back to the pool */
		release(): void;
	}

	/**
	 * COPY FROM options.
	 */
	interface CopyFromOptions {
		/** Column names */
		columns?: string[];
		/** Data format */
		format?: "text" | "csv" | "binary";
		/** Whether the data has a header row */
		header?: boolean;
		/** Column delimiter */
		delimiter?: string;
		/** Data source (string, Blob, or object with text() method) */
		source: string | Blob | { text(): Promise<string> };
		/** Called when COPY starts */
		onCopyStart?: (info: { columns: string[] }) => void;
		/** Called for each chunk */
		onCopyChunk?: (bytes: number) => void;
		/** Called when COPY completes */
		onCopyEnd?: (result: { rowsCopied: number }) => void;
	}

	/**
	 * COPY TO options.
	 */
	interface CopyToOptions {
		/** Column names */
		columns?: string[];
		/** Data format */
		format?: "text" | "csv" | "binary";
		/** Whether to include a header row */
		header?: boolean;
		/** Column delimiter */
		delimiter?: string;
	}

	/**
	 * PostgreSQL error.
	 */
	class PostgresError extends Error {
		name: "PostgresError";
		/** PostgreSQL error code (e.g., "42P01" for undefined table) */
		code: string;
		/** Detailed error message */
		detail?: string;
		/** Hint for fixing the error */
		hint?: string;
	}

	/**
	 * SQLite error.
	 */
	class SQLiteError extends Error {
		name: "SQLiteError";
		/** SQLite error code (e.g., "SQLITE_CONSTRAINT") */
		code: string;
	}

	// ============================================================================
	// KV Store Types
	// ============================================================================

	/**
	 * Key-value store.
	 */
	interface KVStore {
		/**
		 * Set a value for a key.
		 * @param key The key
		 * @param value The value (will be JSON serialized)
		 */
		set(key: string, value: any): void;

		/**
		 * Get a value by key.
		 * @param key The key
		 * @returns The value, or undefined if not found
		 */
		get(key: string): any;

		/**
		 * Delete a key.
		 * @param key The key
		 * @returns True if the key existed
		 */
		delete(key: string): boolean;

		/**
		 * Check if a key exists.
		 * @param key The key
		 * @returns True if the key exists
		 */
		has(key: string): boolean;

		/**
		 * Get all keys.
		 * @returns Array of keys
		 */
		keys(): string[];

		/**
		 * Clear all keys.
		 */
		clear(): void;

		/**
		 * Get the number of keys.
		 */
		readonly size: number;

		/**
		 * Close the store.
		 */
		close(): void;

		/**
		 * Get the path to the store.
		 */
		readonly path: string;

		/**
		 * Check if the store is closed.
		 */
		readonly isClosed: boolean;
	}
}
