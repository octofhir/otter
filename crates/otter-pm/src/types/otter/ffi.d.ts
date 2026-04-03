/**
 * Otter FFI hosted module.
 *
 * New-stack surface:
 * - `dlopen`
 * - `CFunction`
 * - `linkSymbols`
 * - `JSCallback`
 * - `FFIType`
 * - `suffix`
 * - `read.*`
 * - `ptr`
 * - `CString`
 * - `toArrayBuffer`
 * - `toBuffer`
 *
 * Requires `--allow-ffi`.
 */

declare module "otter:ffi" {
	export enum FFIType {
		char = 0,
		int8_t = 1,
		i8 = 1,
		uint8_t = 2,
		u8 = 2,
		int16_t = 3,
		i16 = 3,
		uint16_t = 4,
		u16 = 4,
		int32_t = 5,
		i32 = 5,
		int = 5,
		uint32_t = 6,
		u32 = 6,
		int64_t = 7,
		i64 = 7,
		uint64_t = 8,
		u64 = 8,
		double = 9,
		f64 = 9,
		float = 10,
		f32 = 10,
		bool = 11,
		ptr = 12,
		pointer = 12,
		void = 13,
		cstring = 14,
		i64_fast = 15,
		u64_fast = 16,
		function = 17,
	}

	export type FFITypeOrString =
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

	export type Pointer = number & { readonly __pointer__: unique symbol };

	export interface FFIFunctionDeclaration {
		readonly args?: readonly FFITypeOrString[];
		readonly returns?: FFITypeOrString;
	}

	export interface FFICallableDefinition extends FFIFunctionDeclaration {
		readonly ptr: Pointer | number | JSCallbackHandle | null | undefined;
	}

	type FFIArgType<T extends FFITypeOrString> =
		T extends
			| FFIType.bool
			| "bool"
			? boolean
			: T extends
						| FFIType.cstring
						| "cstring"
				? string | null | undefined
				: T extends
							| FFIType.ptr
							| FFIType.pointer
							| FFIType.function
							| "ptr"
							| "pointer"
							| "function"
							| "fn"
							| "callback"
							| "usize"
					? Pointer | number | JSCallbackHandle | null | undefined
					: number;

	type FFIReturnType<T extends FFITypeOrString | undefined> =
		T extends undefined | FFIType.void | "void"
			? void
			: T extends FFIType.bool | "bool"
				? boolean
				: T extends FFIType.cstring | "cstring"
					? string | null
					: T extends
								| FFIType.ptr
								| FFIType.pointer
								| FFIType.function
								| "ptr"
								| "pointer"
								| "function"
								| "fn"
								| "callback"
								| "usize"
						? Pointer | null
						: number;

	type FFIArgs<T extends readonly FFITypeOrString[] | undefined> =
		T extends readonly []
			? []
			: T extends readonly [...infer Values]
				? { [K in keyof Values]: Values[K] extends FFITypeOrString ? FFIArgType<Values[K]> : never }
				: [];

	export type FFISymbols<T extends Record<string, FFIFunctionDeclaration>> = {
		[K in keyof T]: (
			...args: FFIArgs<T[K]["args"]>
		) => FFIReturnType<T[K]["returns"]>;
	};

	export interface FFILibrary<T extends Record<string, FFIFunctionDeclaration>> {
		readonly symbols: FFISymbols<T>;
		readonly path: string;
		readonly closed: boolean;
		close(): void;
	}

	export interface FFILinkedSymbols<T extends Record<string, FFICallableDefinition>> {
		readonly symbols: {
			[K in keyof T]: (
				...args: FFIArgs<T[K]["args"]>
			) => FFIReturnType<T[K]["returns"]>;
		};
		close(): void;
	}

	export interface JSCallbackHandle {
		readonly ptr: Pointer;
		readonly threadsafe: false;
		close(): void;
	}

	export interface FFIReadNamespace {
		u8(ptr: Pointer | number, offset?: number): number;
		i8(ptr: Pointer | number, offset?: number): number;
		u16(ptr: Pointer | number, offset?: number): number;
		i16(ptr: Pointer | number, offset?: number): number;
		u32(ptr: Pointer | number, offset?: number): number;
		i32(ptr: Pointer | number, offset?: number): number;
		u64(ptr: Pointer | number, offset?: number): number;
		i64(ptr: Pointer | number, offset?: number): number;
		f32(ptr: Pointer | number, offset?: number): number;
		f64(ptr: Pointer | number, offset?: number): number;
		ptr(ptr: Pointer | number, offset?: number): Pointer | null;
		intptr(ptr: Pointer | number, offset?: number): number;
		cstring(ptr: Pointer | number, offset?: number): string;
	}

	export function dlopen<T extends Record<string, FFIFunctionDeclaration>>(
		path: string,
		declarations: T,
	): FFILibrary<T>;

	export function CFunction<T extends FFICallableDefinition>(
		definition: T,
	): (...args: FFIArgs<T["args"]>) => FFIReturnType<T["returns"]>;

	export function linkSymbols<T extends Record<string, FFICallableDefinition>>(
		symbols: T,
	): FFILinkedSymbols<T>;

	export function JSCallback<T extends FFIFunctionDeclaration>(
		callback: (...args: FFIArgs<T["args"]>) => FFIReturnType<T["returns"]>,
		definition: T,
	): JSCallbackHandle;

	export function ptr(
		value: Pointer | number | ArrayBuffer | ArrayBufferView | null | undefined,
		byteOffset?: number,
	): Pointer | null;

	export function CString(
		ptr: Pointer | number | null | undefined,
		offset?: number,
	): string | null;

	export function toArrayBuffer(
		ptr: Pointer | number,
		byteOffset: number | undefined,
		byteLength: number,
	): ArrayBuffer;

	export function toBuffer(
		ptr: Pointer | number,
		byteOffset: number | undefined,
		byteLength: number,
	): ArrayBuffer;

	export const suffix: string;
	export const read: FFIReadNamespace;

	export interface FFIModuleNamespace {
		dlopen: typeof dlopen;
		CFunction: typeof CFunction;
		linkSymbols: typeof linkSymbols;
		JSCallback: typeof JSCallback;
		ptr: typeof ptr;
		CString: typeof CString;
		toArrayBuffer: typeof toArrayBuffer;
		toBuffer: typeof toBuffer;
		suffix: typeof suffix;
		read: typeof read;
		FFIType: typeof FFIType;
	}

	const ffi: FFIModuleNamespace;
	export default ffi;
}
