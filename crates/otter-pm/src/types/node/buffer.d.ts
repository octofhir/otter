/**
 * The `node:buffer` module provides a way to handle binary data directly.
 * @module node:buffer
 */
declare module "node:buffer" {
    /**
     * Supported buffer encodings.
     */
    export type BufferEncoding = "utf8" | "utf-8" | "base64" | "hex";

    /**
     * The Buffer class is a global type for dealing with binary data directly.
     * It can be constructed in a variety of ways.
     */
    export class Buffer extends Uint8Array {
        /**
         * Allocates a new Buffer of size bytes.
         * @param size The desired length of the new Buffer
         * @param fill A value to pre-fill the Buffer with (default: 0)
         * @param encoding The encoding of fill if fill is a string (default: 'utf8')
         * @returns A new Buffer
         * @example
         * const buf = Buffer.alloc(5);
         * // Creates a Buffer of length 5, filled with zeros
         */
        static alloc(size: number, fill?: number | string, encoding?: BufferEncoding): Buffer;

        /**
         * Allocates a new Buffer using data. If data is a string, encoding specifies its encoding.
         * @param data Data to create Buffer from
         * @param encoding The encoding if data is a string (default: 'utf8')
         * @returns A new Buffer
         * @example
         * const buf1 = Buffer.from('hello', 'utf8');
         * const buf2 = Buffer.from([0x62, 0x75, 0x66, 0x66, 0x65, 0x72]);
         * const buf3 = Buffer.from('aGVsbG8=', 'base64');
         */
        static from(data: string | number[] | Buffer | ArrayBuffer | Uint8Array, encoding?: BufferEncoding): Buffer;

        /**
         * Returns a new Buffer which is the result of concatenating all the Buffer instances together.
         * @param list Array of Buffer instances to concat
         * @param totalLength Total length of the resulting Buffer
         * @returns A new concatenated Buffer
         * @example
         * const buf1 = Buffer.from('Hello ');
         * const buf2 = Buffer.from('World');
         * const buf3 = Buffer.concat([buf1, buf2]);
         */
        static concat(list: Buffer[], totalLength?: number): Buffer;

        /**
         * Returns true if obj is a Buffer, false otherwise.
         * @param obj Object to test
         * @returns True if obj is a Buffer
         */
        static isBuffer(obj: unknown): obj is Buffer;

        /**
         * Returns the byte length of a string when encoded using encoding.
         * @param string String to measure
         * @param encoding The encoding (default: 'utf8')
         * @returns The byte length
         */
        static byteLength(string: string | Buffer, encoding?: BufferEncoding): number;

        /**
         * Decodes the Buffer to a string according to the specified encoding.
         * @param encoding The encoding to use (default: 'utf8')
         * @param start Byte offset to start decoding at (default: 0)
         * @param end Byte offset to stop decoding at (default: buffer.length)
         * @returns The decoded string
         * @example
         * const buf = Buffer.from('hello', 'utf8');
         * buf.toString('base64'); // 'aGVsbG8='
         */
        toString(encoding?: BufferEncoding, start?: number, end?: number): string;

        /**
         * Returns a new Buffer that references the same memory as the original,
         * but offset and cropped by the start and end indices.
         * @param start Starting index (default: 0)
         * @param end Ending index (default: buffer.length)
         * @returns A new Buffer sharing memory with original
         */
        slice(start?: number, end?: number): Buffer;

        /**
         * Copies data from a region of this Buffer to a region in target.
         * @param target Buffer to copy into
         * @param targetStart Offset within target at which to begin writing (default: 0)
         * @param sourceStart Offset within this Buffer to start copying from (default: 0)
         * @param sourceEnd Offset within this Buffer to stop copying (default: buffer.length)
         * @returns The number of bytes copied
         */
        copy(target: Buffer, targetStart?: number, sourceStart?: number, sourceEnd?: number): number;

        /**
         * Returns true if both this and other have exactly the same bytes, false otherwise.
         * @param other Buffer to compare to
         * @returns True if buffers are equal
         */
        equals(other: Buffer): boolean;

        /**
         * Compares this buffer with other and returns a number indicating sort order.
         * @param other Buffer to compare to
         * @returns 0 if equal, -1 if this sorts before other, 1 if this sorts after
         */
        compare(other: Buffer): -1 | 0 | 1;

        /**
         * The size of the Buffer in bytes.
         */
        readonly length: number;
    }
}

// Also support the 'buffer' module (without node: prefix)
declare module "buffer" {
    export * from "node:buffer";
}

// Add Buffer to global scope
declare global {
    const Buffer: typeof import("node:buffer").Buffer;
}
