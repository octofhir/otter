// Stub implementation for bytecode cache when using system JSC
// System JSC doesn't expose the bytecode generation APIs

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

typedef void* JSContextRef;

struct OtterBytecodeResult {
    bool success;
    const uint8_t* data;
    size_t size;
    char error_message[256];
};

bool otter_generate_program_bytecode(
    JSContextRef ctx,
    const char* source,
    size_t source_len,
    const char* filename,
    size_t filename_len,
    struct OtterBytecodeResult* out)
{
    (void)ctx;
    (void)source;
    (void)source_len;
    (void)filename;
    (void)filename_len;

    if (out) {
        out->success = false;
        out->data = NULL;
        out->size = 0;
        strncpy(out->error_message,
            "Bytecode generation not available with system JSC",
            sizeof(out->error_message) - 1);
        out->error_message[sizeof(out->error_message) - 1] = '\0';
    }
    return false;
}

bool otter_generate_program_bytecode_to_file(
    JSContextRef ctx,
    const char* source,
    size_t source_len,
    const char* filename,
    size_t filename_len,
    const char* output_path,
    size_t output_path_len,
    struct OtterBytecodeResult* out)
{
    (void)ctx;
    (void)source;
    (void)source_len;
    (void)filename;
    (void)filename_len;
    (void)output_path;
    (void)output_path_len;

    if (out) {
        out->success = false;
        out->data = NULL;
        out->size = 0;
        strncpy(out->error_message,
            "Bytecode generation not available with system JSC",
            sizeof(out->error_message) - 1);
        out->error_message[sizeof(out->error_message) - 1] = '\0';
    }
    return false;
}

bool otter_generate_module_bytecode_to_file(
    JSContextRef ctx,
    const char* source,
    size_t source_len,
    const char* filename,
    size_t filename_len,
    const char* output_path,
    size_t output_path_len,
    struct OtterBytecodeResult* out)
{
    (void)ctx;
    (void)source;
    (void)source_len;
    (void)filename;
    (void)filename_len;
    (void)output_path;
    (void)output_path_len;

    if (out) {
        out->success = false;
        out->data = NULL;
        out->size = 0;
        strncpy(out->error_message,
            "Bytecode generation not available with system JSC",
            sizeof(out->error_message) - 1);
        out->error_message[sizeof(out->error_message) - 1] = '\0';
    }
    return false;
}

bool otter_evaluate_with_cache(
    JSContextRef ctx,
    const char* source,
    size_t source_len,
    const char* filename,
    size_t filename_len,
    const char* bytecode_path,
    size_t bytecode_path_len,
    struct OtterBytecodeResult* out)
{
    (void)ctx;
    (void)source;
    (void)source_len;
    (void)filename;
    (void)filename_len;
    (void)bytecode_path;
    (void)bytecode_path_len;

    if (out) {
        out->success = false;
        out->data = NULL;
        out->size = 0;
        strncpy(out->error_message,
            "Bytecode cache evaluation not available with system JSC",
            sizeof(out->error_message) - 1);
        out->error_message[sizeof(out->error_message) - 1] = '\0';
    }
    return false;
}
