/**
 * Otter FFI (Foreign Function Interface)
 *
 * Load and call native C/C++ shared libraries directly from JavaScript/TypeScript.
 * API follows common runtime FFI conventions.
 *
 * Requires --allow-ffi permission flag.
 *
 * @example
 * ```typescript
 * import { dlopen, FFIType, suffix } from "otter:ffi";
 *
 * const lib = dlopen(`libsqlite3.${suffix}`, {
 *   sqlite3_libversion: { args: [], returns: FFIType.cstring },
 *   sqlite3_open: { args: [FFIType.cstring, FFIType.ptr], returns: FFIType.i32 },
 * });
 *
 * console.log(lib.symbols.sqlite3_libversion()); // "3.45.0"
 * lib.close();
 * ```
 */

declare module "otter:ffi" {
	// ============================================================================
	// FFI Type System
	// ============================================================================

	/**
	 * FFI type enum — describes C types for function signatures.
	 */
	export enum FFIType {
		char = 0,
		/** Signed 8-bit integer */
		int8_t = 1,
		i8 = 1,
		/** Unsigned 8-bit integer */
		uint8_t = 2,
		u8 = 2,
		/** Signed 16-bit integer */
		int16_t = 3,
		i16 = 3,
		/** Unsigned 16-bit integer */
		uint16_t = 4,
		u16 = 4,
		/** Signed 32-bit integer */
		int32_t = 5,
		i32 = 5,
		int = 5,
		/** Unsigned 32-bit integer */
		uint32_t = 6,
		u32 = 6,
		/** Signed 64-bit integer (returns as bigint) */
		int64_t = 7,
		i64 = 7,
		/** Unsigned 64-bit integer (returns as bigint) */
		uint64_t = 8,
		u64 = 8,
		/** 64-bit floating point */
		double = 9,
		f64 = 9,
		/** 32-bit floating point */
		float = 10,
		f32 = 10,
		/** Boolean */
		bool = 11,
		/** Pointer (represented as JS number) */
		ptr = 12,
		pointer = 12,
		/** Void (for return types only) */
		void = 13,
		/** C string (null-terminated UTF-8) */
		cstring = 14,
		/** i64 that coerces to number if it fits, bigint otherwise */
		i64_fast = 15,
		/** u64 that coerces to number if it fits, bigint otherwise */
		u64_fast = 16,
		/** Function pointer */
		function = 17,
	}

	/**
	 * FFI type specifier — accepts both enum values and string aliases.
	 */
	type FFITypeOrString =
		| FFIType
		| "char"
		| "int8_t"
		| "i8"
		| "uint8_t"
		| "u8"
		| "int16_t"
		| "i16"
		| "uint16_t"
		| "u16"
		| "int32_t"
		| "i32"
		| "int"
		| "uint32_t"
		| "u32"
		| "int64_t"
		| "i64"
		| "uint64_t"
		| "u64"
		| "double"
		| "f64"
		| "float"
		| "f32"
		| "bool"
		| "ptr"
		| "pointer"
		| "void"
		| "cstring"
		| "i64_fast"
		| "u64_fast"
		| "function"
		| "fn"
		| "callback"
		| "usize";

	// ============================================================================
	// Pointer Type
	// ============================================================================

	/**
	 * Branded pointer type — a number representing a native memory address.
	 *
	 * JS numbers have 53 bits of mantissa precision, which covers the usable
	 * address space on all current 64-bit platforms (48-52 bit virtual addresses).
	 */
	type Pointer = number & { readonly __pointer__: unique symbol };

	// ============================================================================
	// Function Signatures
	// ============================================================================

	/**
	 * Describes a native function's signature for dlopen.
	 */
	interface FFIFunction {
		/** Parameter types (default: [] — no arguments) */
		readonly args?: readonly FFITypeOrString[];
		/** Return type (default: FFIType.void) */
		readonly returns?: FFITypeOrString;
		/** Use an explicit function pointer instead of dlsym lookup */
		readonly ptr?: Pointer | bigint;
	}

	// ============================================================================
	// Type Mapping (FFIType -> JS types)
	// ============================================================================

	type FFITypeToArgType<T extends FFITypeOrString> = T extends
		| FFIType.i8
		| FFIType.u8
		| FFIType.i16
		| FFIType.u16
		| FFIType.i32
		| FFIType.u32
		| FFIType.f32
		| FFIType.f64
		| FFIType.char
		| "char"
		| "i8"
		| "int8_t"
		| "u8"
		| "uint8_t"
		| "i16"
		| "int16_t"
		| "u16"
		| "uint16_t"
		| "i32"
		| "int32_t"
		| "int"
		| "u32"
		| "uint32_t"
		| "f32"
		| "float"
		| "f64"
		| "double"
		? number
		: T extends FFIType.bool | "bool"
			? boolean
			: T extends
						| FFIType.i64
						| FFIType.u64
						| FFIType.i64_fast
						| FFIType.u64_fast
						| "i64"
						| "int64_t"
						| "u64"
						| "uint64_t"
						| "i64_fast"
						| "u64_fast"
						| "usize"
				? number | bigint
				: T extends
							| FFIType.ptr
							| FFIType.cstring
							| FFIType.function
							| "ptr"
							| "pointer"
							| "cstring"
							| "function"
							| "fn"
							| "callback"
					? Pointer | TypedArray | null
					: unknown;

	type FFITypeToReturnType<T extends FFITypeOrString> = T extends
		| FFIType.i8
		| FFIType.u8
		| FFIType.i16
		| FFIType.u16
		| FFIType.i32
		| FFIType.u32
		| FFIType.f32
		| FFIType.f64
		| FFIType.char
		| "char"
		| "i8"
		| "int8_t"
		| "u8"
		| "uint8_t"
		| "i16"
		| "int16_t"
		| "u16"
		| "uint16_t"
		| "i32"
		| "int32_t"
		| "int"
		| "u32"
		| "uint32_t"
		| "f32"
		| "float"
		| "f64"
		| "double"
		? number
		: T extends FFIType.bool | "bool"
			? boolean
			: T extends FFIType.i64 | FFIType.u64 | "i64" | "int64_t" | "u64" | "uint64_t" | "usize"
				? bigint
				: T extends FFIType.i64_fast | FFIType.u64_fast | "i64_fast" | "u64_fast"
					? number | bigint
					: T extends FFIType.cstring | "cstring"
						? CString
						: T extends FFIType.ptr | FFIType.function | "ptr" | "pointer" | "function" | "fn" | "callback"
							? Pointer | null
							: T extends FFIType.void | "void"
								? undefined
								: unknown;

	/** Map FFIFunction declarations to typed callable functions */
	type ConvertFns<T extends Record<string, FFIFunction>> = {
		[K in keyof T]: T[K]["args"] extends readonly FFITypeOrString[]
			? (
					...args: {
						[I in keyof T[K]["args"]]: T[K]["args"][I] extends FFITypeOrString
							? FFITypeToArgType<T[K]["args"][I]>
							: unknown;
					}
				) => T[K]["returns"] extends FFITypeOrString
					? FFITypeToReturnType<T[K]["returns"]>
					: undefined
			: () => T[K]["returns"] extends FFITypeOrString
				? FFITypeToReturnType<T[K]["returns"]>
				: undefined;
	};

	// ============================================================================
	// Library
	// ============================================================================

	/**
	 * A loaded native library with typed symbol bindings.
	 */
	interface Library<T extends Record<string, FFIFunction>> {
		/** Bound native functions with full type inference */
		readonly symbols: ConvertFns<T>;
		/**
		 * Unload the library and invalidate all symbol bindings.
		 * Calling symbols after close() is undefined behavior.
		 */
		close(): void;
	}

	// ============================================================================
	// Core Functions
	// ============================================================================

	/**
	 * Load a shared library and bind native function symbols.
	 *
	 * @param path Path to the shared library (e.g., `libsqlite3.${suffix}`)
	 * @param symbols Symbol declarations mapping names to FFI signatures
	 * @returns A Library object with typed .symbols and .close()
	 *
	 * @example
	 * ```typescript
	 * import { dlopen, FFIType, suffix } from "otter:ffi";
	 *
	 * const lib = dlopen(`./mylib.${suffix}`, {
	 *   add: { args: [FFIType.i32, FFIType.i32], returns: FFIType.i32 },
	 *   greet: { args: [FFIType.cstring], returns: FFIType.void },
	 * });
	 *
	 * const result = lib.symbols.add(2, 3); // 5
	 * lib.close();
	 * ```
	 */
	export function dlopen<T extends Record<string, FFIFunction>>(
		path: string,
		symbols: T,
	): Library<T>;

	/**
	 * Create a callable function from a raw function pointer.
	 *
	 * @example
	 * ```typescript
	 * const fn = CFunction({ ptr: myPtr, args: ["i32"], returns: "i32" });
	 * fn(42);
	 * fn.close();
	 * ```
	 */
	export function CFunction(
		definition: FFIFunction & { ptr: Pointer | bigint },
	): ((...args: any[]) => any) & { close(): void };

	/**
	 * Bind multiple function pointers at once.
	 *
	 * @example
	 * ```typescript
	 * const lib = linkSymbols({
	 *   add: { ptr: addPtr, args: ["i32", "i32"], returns: "i32" },
	 *   mul: { ptr: mulPtr, args: ["f64", "f64"], returns: "f64" },
	 * });
	 * lib.symbols.add(1, 2);
	 * ```
	 */
	export function linkSymbols<T extends Record<string, FFIFunction & { ptr: Pointer | bigint }>>(
		symbols: T,
	): Library<T>;

	// ============================================================================
	// CString
	// ============================================================================

	/**
	 * A null-terminated C string read from native memory.
	 *
	 * CString clones the data from the pointer into JS memory on construction,
	 * so it remains valid after the underlying C memory is freed.
	 *
	 * @example
	 * ```typescript
	 * const str = new CString(ptr);
	 * console.log(str.toString()); // "hello"
	 * console.log(str.arrayBuffer); // ArrayBuffer of UTF-8 bytes
	 * ```
	 */
	export class CString extends String {
		/**
		 * @param ptr Pointer to a null-terminated C string
		 * @param byteOffset Optional byte offset from ptr
		 * @param byteLength Optional byte length (if omitted, scans for null terminator)
		 */
		constructor(ptr: Pointer, byteOffset?: number, byteLength?: number);

		/** The raw pointer this CString was constructed from */
		readonly ptr: Pointer;
		/** Byte offset from ptr */
		readonly byteOffset?: number;
		/** Byte length of the string */
		readonly byteLength?: number;
		/** The raw UTF-8 bytes as an ArrayBuffer */
		readonly arrayBuffer: ArrayBuffer;
	}

	// ============================================================================
	// JSCallback
	// ============================================================================

	/**
	 * Wraps a JavaScript function as a C-callable function pointer.
	 *
	 * The returned `.ptr` can be passed to native code that expects a callback.
	 * Call `.close()` when done to free the associated memory.
	 *
	 * @example
	 * ```typescript
	 * const cb = new JSCallback(
	 *   (x: number) => x * 2,
	 *   { args: [FFIType.i32], returns: FFIType.i32 },
	 * );
	 *
	 * // Pass cb.ptr to a native function that expects int(*)(int)
	 * nativeFunc(cb.ptr);
	 *
	 * cb.close(); // Free when done
	 * ```
	 */
	export class JSCallback {
		constructor(
			callback: (...args: any[]) => any,
			definition: {
				args: FFITypeOrString[];
				returns: FFITypeOrString;
				threadsafe?: boolean;
			},
		);

		/** C function pointer — becomes null after close() */
		readonly ptr: Pointer | null;
		/** Whether this callback is threadsafe */
		readonly threadsafe: boolean;
		/** Free the callback memory and invalidate ptr */
		close(): void;
	}

	// ============================================================================
	// Pointer Operations
	// ============================================================================

	/**
	 * Get the raw pointer address of a TypedArray's underlying buffer.
	 *
	 * @param view A TypedArray, DataView, or ArrayBuffer
	 * @param byteOffset Optional byte offset into the buffer
	 * @returns The raw memory address as a Pointer
	 *
	 * @example
	 * ```typescript
	 * const buf = new Uint8Array([1, 2, 3, 4]);
	 * const p = ptr(buf);
	 * console.log(read.u8(p, 0)); // 1
	 * ```
	 */
	export function ptr(
		view: TypedArray | DataView | ArrayBufferLike,
		byteOffset?: number,
	): Pointer;

	/**
	 * Create an ArrayBuffer from a raw pointer (zero-copy view).
	 *
	 * **Warning**: The caller is responsible for ensuring the pointer remains valid
	 * for the lifetime of the returned ArrayBuffer.
	 *
	 * @param ptr Raw pointer address
	 * @param byteOffset Byte offset from ptr
	 * @param byteLength Number of bytes (if omitted, scans for null terminator)
	 */
	export function toArrayBuffer(
		ptr: Pointer,
		byteOffset?: number,
		byteLength?: number,
	): ArrayBuffer;

	/**
	 * Create a Buffer from a raw pointer (zero-copy view).
	 *
	 * @param ptr Raw pointer address
	 * @param byteOffset Byte offset from ptr
	 * @param byteLength Number of bytes (if omitted, scans for null terminator)
	 */
	export function toBuffer(
		ptr: Pointer,
		byteOffset?: number,
		byteLength?: number,
	): Buffer;

	// ============================================================================
	// Direct Memory Reads
	// ============================================================================

	/**
	 * Direct memory read functions.
	 * Faster than creating a DataView for short-lived reads.
	 *
	 * @example
	 * ```typescript
	 * import { read, ptr } from "otter:ffi";
	 *
	 * const buf = new Float64Array([3.14, 2.71]);
	 * const p = ptr(buf);
	 * console.log(read.f64(p, 0)); // 3.14
	 * console.log(read.f64(p, 8)); // 2.71
	 * ```
	 */
	export namespace read {
		function u8(ptr: Pointer, byteOffset?: number): number;
		function i8(ptr: Pointer, byteOffset?: number): number;
		function u16(ptr: Pointer, byteOffset?: number): number;
		function i16(ptr: Pointer, byteOffset?: number): number;
		function u32(ptr: Pointer, byteOffset?: number): number;
		function i32(ptr: Pointer, byteOffset?: number): number;
		function f32(ptr: Pointer, byteOffset?: number): number;
		function f64(ptr: Pointer, byteOffset?: number): number;
		function u64(ptr: Pointer, byteOffset?: number): bigint;
		function i64(ptr: Pointer, byteOffset?: number): bigint;
		function ptr(ptr: Pointer, byteOffset?: number): Pointer;
		function intptr(ptr: Pointer, byteOffset?: number): number;
	}

	// ============================================================================
	// Platform Constants
	// ============================================================================

	/**
	 * Platform-specific shared library file extension.
	 *
	 * - macOS: `"dylib"`
	 * - Linux: `"so"`
	 * - Windows: `"dll"`
	 *
	 * @example
	 * ```typescript
	 * const lib = dlopen(`./mylib.${suffix}`, { ... });
	 * ```
	 */
	export const suffix: "dylib" | "so" | "dll";

	// ============================================================================
	// TypedArray helper type
	// ============================================================================

	type TypedArray =
		| Uint8Array
		| Int8Array
		| Uint16Array
		| Int16Array
		| Uint32Array
		| Int32Array
		| Float32Array
		| Float64Array
		| BigInt64Array
		| BigUint64Array;
}
