// node:string_decoder module implementation
// Provides StringDecoder class for decoding Buffer to strings with proper handling
// of incomplete multi-byte sequences across chunk boundaries.

(function() {
    'use strict';

    // Encoding name normalization
    function normalizeEncoding(enc) {
        if (!enc) return 'utf8';
        const lower = enc.toLowerCase().replace(/[-_]/g, '');
        switch (lower) {
            case 'utf8':
            case 'utf-8':
                return 'utf8';
            case 'utf16le':
            case 'utf16-le':
            case 'ucs2':
            case 'ucs-2':
                return 'utf16le';
            case 'latin1':
            case 'binary':
            case 'iso88591':
            case 'iso-8859-1':
                return 'latin1';
            case 'base64':
                return 'base64';
            case 'base64url':
                return 'base64url';
            case 'hex':
                return 'hex';
            case 'ascii':
                return 'ascii';
            default:
                throw new Error(`Unknown encoding: ${enc}`);
        }
    }

    // Get the number of bytes for a UTF-8 character based on first byte
    function utf8ByteLength(byte) {
        if ((byte & 0x80) === 0) return 1;        // 0xxxxxxx - ASCII
        if ((byte & 0xe0) === 0xc0) return 2;     // 110xxxxx
        if ((byte & 0xf0) === 0xe0) return 3;     // 1110xxxx
        if ((byte & 0xf8) === 0xf0) return 4;     // 11110xxx
        return 1; // Invalid byte, treat as single
    }

    // Check if a byte is a UTF-8 continuation byte
    function isUtf8Continuation(byte) {
        return (byte & 0xc0) === 0x80;
    }

    class StringDecoder {
        constructor(encoding) {
            this.encoding = normalizeEncoding(encoding);
            // Buffer to store incomplete multi-byte sequences
            this._incomplete = new Uint8Array(4);
            this._incompleteLength = 0;
            this._expectedLength = 0;
        }

        /**
         * Write a buffer and return the decoded string.
         * May hold back incomplete multi-byte sequences.
         */
        write(buffer) {
            if (buffer == null || buffer.length === 0) {
                return '';
            }

            // Handle Otter's Buffer format {type: "Buffer", data: [...]}
            if (buffer && buffer.type === 'Buffer' && Array.isArray(buffer.data)) {
                buffer = new Uint8Array(buffer.data);
            } else if (buffer instanceof ArrayBuffer) {
                buffer = new Uint8Array(buffer);
            } else if (typeof buffer === 'string') {
                return buffer; // Already a string
            } else if (Array.isArray(buffer)) {
                buffer = new Uint8Array(buffer);
            } else if (!ArrayBuffer.isView(buffer)) {
                throw new TypeError('Argument must be a Buffer or Uint8Array');
            }

            switch (this.encoding) {
                case 'utf8':
                    return this._writeUtf8(buffer);
                case 'utf16le':
                    return this._writeUtf16le(buffer);
                case 'latin1':
                case 'ascii':
                case 'hex':
                case 'base64':
                case 'base64url':
                    return this._writeSingleByte(buffer);
                default:
                    return this._writeSingleByte(buffer);
            }
        }

        /**
         * Return any remaining incomplete bytes as a string.
         */
        end(buffer) {
            let result = '';
            if (buffer && buffer.length > 0) {
                result = this.write(buffer);
            }

            // Flush any incomplete data
            if (this._incompleteLength > 0) {
                switch (this.encoding) {
                    case 'utf8':
                        // Output replacement character for incomplete UTF-8
                        result += '\ufffd';
                        break;
                    case 'utf16le':
                        // Output whatever partial data we have
                        if (this._incompleteLength === 1) {
                            result += String.fromCharCode(this._incomplete[0]);
                        }
                        break;
                    default:
                        // Single-byte encodings should have no incomplete data
                        break;
                }
                this._incompleteLength = 0;
                this._expectedLength = 0;
            }

            return result;
        }

        _writeUtf8(buffer) {
            let result = '';
            let start = 0;

            // First, try to complete any incomplete sequence from previous chunk
            if (this._incompleteLength > 0) {
                const needed = this._expectedLength - this._incompleteLength;
                const available = Math.min(needed, buffer.length);

                // Copy available bytes to incomplete buffer
                for (let i = 0; i < available; i++) {
                    this._incomplete[this._incompleteLength + i] = buffer[i];
                }
                this._incompleteLength += available;
                start = available;

                // Check if we now have a complete sequence
                if (this._incompleteLength >= this._expectedLength) {
                    // Decode the complete sequence
                    const bytes = this._incomplete.slice(0, this._expectedLength);
                    result += new TextDecoder('utf-8').decode(bytes);
                    this._incompleteLength = 0;
                    this._expectedLength = 0;
                } else {
                    // Still incomplete, need more data
                    return '';
                }
            }

            // Find the end of complete UTF-8 sequences
            let end = buffer.length;
            if (end > start) {
                // Check last few bytes for incomplete sequence
                let lastComplete = end;
                for (let i = Math.min(3, end - start); i > 0; i--) {
                    const byte = buffer[end - i];
                    if (!isUtf8Continuation(byte)) {
                        // This might be the start of a multi-byte sequence
                        const expectedLen = utf8ByteLength(byte);
                        if (expectedLen > i) {
                            // Incomplete sequence at end
                            lastComplete = end - i;
                            this._expectedLength = expectedLen;
                            // Save the incomplete bytes
                            for (let j = 0; j < i; j++) {
                                this._incomplete[j] = buffer[lastComplete + j];
                            }
                            this._incompleteLength = i;
                            break;
                        }
                        break;
                    }
                }
                end = lastComplete;
            }

            // Decode the complete portion
            if (end > start) {
                const slice = buffer.slice ? buffer.slice(start, end) : buffer.subarray(start, end);
                result += new TextDecoder('utf-8').decode(slice);
            }

            return result;
        }

        _writeUtf16le(buffer) {
            let result = '';
            let start = 0;

            // Handle incomplete byte from previous chunk
            if (this._incompleteLength === 1) {
                if (buffer.length > 0) {
                    const codeUnit = this._incomplete[0] | (buffer[0] << 8);
                    result += String.fromCharCode(codeUnit);
                    start = 1;
                    this._incompleteLength = 0;
                } else {
                    return '';
                }
            }

            // Process pairs of bytes manually (TextDecoder may not support utf-16le)
            const remaining = buffer.length - start;
            const completeLen = remaining - (remaining % 2);

            for (let i = start; i < start + completeLen; i += 2) {
                const codeUnit = buffer[i] | (buffer[i + 1] << 8);
                result += String.fromCharCode(codeUnit);
            }

            // Save any trailing odd byte
            if (remaining % 2 === 1) {
                this._incomplete[0] = buffer[buffer.length - 1];
                this._incompleteLength = 1;
            }

            return result;
        }

        _writeSingleByte(buffer) {
            // Single-byte encodings - no incomplete sequences possible
            switch (this.encoding) {
                case 'latin1':
                    return Array.from(buffer).map(b => String.fromCharCode(b)).join('');
                case 'ascii':
                    return Array.from(buffer).map(b => String.fromCharCode(b & 0x7f)).join('');
                case 'hex':
                    return Array.from(buffer).map(b => b.toString(16).padStart(2, '0')).join('');
                case 'base64':
                    // For base64, we need to handle incomplete chunks
                    return this._writeBase64(buffer);
                case 'base64url':
                    return this._writeBase64(buffer, true);
                default:
                    return new TextDecoder(this.encoding).decode(buffer);
            }
        }

        _writeBase64(buffer, urlSafe = false) {
            // Base64 decoding must happen in 4-byte chunks
            // This is actually for encoding buffers to base64 strings
            if (typeof btoa === 'function') {
                const binary = Array.from(buffer).map(b => String.fromCharCode(b)).join('');
                let b64 = btoa(binary);
                if (urlSafe) {
                    b64 = b64.replace(/\+/g, '-').replace(/\//g, '_').replace(/=/g, '');
                }
                return b64;
            }
            // Fallback - return hex if btoa not available
            return Array.from(buffer).map(b => b.toString(16).padStart(2, '0')).join('');
        }

        /**
         * @deprecated Use encoding property instead
         */
        get lastChar() {
            return this._incomplete.slice(0, this._incompleteLength);
        }

        /**
         * @deprecated
         */
        get lastNeed() {
            return this._expectedLength - this._incompleteLength;
        }

        /**
         * @deprecated
         */
        get lastTotal() {
            return this._expectedLength;
        }
    }

    const stringDecoderModule = {
        StringDecoder,
    };
    stringDecoderModule.default = stringDecoderModule;

    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('string_decoder', stringDecoderModule);
    }
})();
