// Bytecode cache API for JSC
// Provides C-compatible wrappers for JSC's bytecode generation/caching functions

#include <JavaScriptCore/APICast.h>
#include <JavaScriptCore/JSGlobalObjectInlines.h>
#include <JavaScriptCore/VM.h>
#include <JavaScriptCore/Completion.h>
#include <JavaScriptCore/SourceCode.h>
#include <JavaScriptCore/CachedBytecode.h>
#include <JavaScriptCore/BytecodeCacheError.h>
#include <wtf/FileSystem.h>

extern "C" {

/// Result of bytecode generation
struct OtterBytecodeResult {
    bool success;
    const uint8_t* data;
    size_t size;
    char error_message[256];
};

/// Generate bytecode for a program (script) source code
/// Returns bytecode data that can be cached to disk
///
/// Parameters:
/// - ctx: JSC context
/// - source: JavaScript source code (UTF-8)
/// - source_len: Length of source code
/// - filename: Source filename for error messages (UTF-8)
/// - filename_len: Length of filename
/// - out: Output result structure
///
/// The caller must call otter_bytecode_free() to release the bytecode data
bool otter_generate_program_bytecode(
    JSContextRef ctx,
    const char* source,
    size_t source_len,
    const char* filename,
    size_t filename_len,
    OtterBytecodeResult* out)
{
    if (!ctx || !source || !out) {
        if (out) {
            out->success = false;
            snprintf(out->error_message, sizeof(out->error_message), "Invalid arguments");
        }
        return false;
    }

    auto* globalObject = toJS(ctx);
    if (!globalObject) {
        out->success = false;
        snprintf(out->error_message, sizeof(out->error_message), "Invalid context");
        return false;
    }

    JSC::VM& vm = getVM(globalObject);

    // Create source code
    WTF::String sourceString = WTF::String::fromUTF8(source, source_len);
    WTF::String filenameString = filename ? WTF::String::fromUTF8(filename, filename_len) : WTF::String("script.js"_s);

    JSC::SourceCode sourceCode = JSC::makeSource(
        sourceString,
        JSC::SourceOrigin(WTF::URL(filenameString)),
        JSC::SourceTaintedOrigin::Untainted,
        filenameString
    );

    // Generate bytecode
    JSC::BytecodeCacheError error;

    // We need a file handle for the API, but we'll use a temporary approach
    // Create a memory-mapped temp file or use in-memory approach
    // For now, we'll use a simpler approach with generateProgramBytecode

    // Note: The JSC API requires a FileHandle for writing bytecode
    // This is a limitation we need to work around
    // For build-time generation, we can write to a temp file and read it back

    out->success = false;
    snprintf(out->error_message, sizeof(out->error_message),
        "Bytecode generation requires file handle - use otter_generate_program_bytecode_to_file instead");
    return false;
}

/// Generate bytecode for a program and write directly to a file
/// This is the preferred method for build-time bytecode generation
///
/// Parameters:
/// - ctx: JSC context
/// - source: JavaScript source code (UTF-8)
/// - source_len: Length of source code
/// - filename: Source filename for error messages (UTF-8)
/// - filename_len: Length of filename
/// - output_path: Path to write bytecode file (UTF-8)
/// - output_path_len: Length of output path
/// - out: Output result structure (data/size will be 0, only success/error populated)
bool otter_generate_program_bytecode_to_file(
    JSContextRef ctx,
    const char* source,
    size_t source_len,
    const char* filename,
    size_t filename_len,
    const char* output_path,
    size_t output_path_len,
    OtterBytecodeResult* out)
{
    if (!ctx || !source || !output_path || !out) {
        if (out) {
            out->success = false;
            snprintf(out->error_message, sizeof(out->error_message), "Invalid arguments");
        }
        return false;
    }

    auto* globalObject = toJS(ctx);
    if (!globalObject) {
        out->success = false;
        snprintf(out->error_message, sizeof(out->error_message), "Invalid context");
        return false;
    }

    JSC::VM& vm = getVM(globalObject);

    // Create source code
    WTF::String sourceString = WTF::String::fromUTF8(source, source_len);
    WTF::String filenameString = filename ? WTF::String::fromUTF8(filename, filename_len) : WTF::String("script.js"_s);
    WTF::String outputPathString = WTF::String::fromUTF8(output_path, output_path_len);

    JSC::SourceCode sourceCode = JSC::makeSource(
        sourceString,
        JSC::SourceOrigin(WTF::URL(filenameString)),
        JSC::SourceTaintedOrigin::Untainted,
        filenameString
    );

    // Open output file
    auto fileHandle = FileSystem::openFile(outputPathString, FileSystem::FileOpenMode::Truncate);
    if (!FileSystem::isHandleValid(fileHandle)) {
        out->success = false;
        snprintf(out->error_message, sizeof(out->error_message), "Failed to open output file: %s", output_path);
        return false;
    }

    // Generate bytecode
    JSC::BytecodeCacheError error;
    RefPtr<JSC::CachedBytecode> bytecode = JSC::generateProgramBytecode(vm, sourceCode, fileHandle, error);

    FileSystem::closeFile(fileHandle);

    if (!bytecode || error.isValid()) {
        out->success = false;
        if (error.isValid()) {
            WTF::String errorMsg = error.message();
            auto utf8 = errorMsg.utf8();
            snprintf(out->error_message, sizeof(out->error_message), "%s", utf8.data());
        } else {
            snprintf(out->error_message, sizeof(out->error_message), "Bytecode generation failed");
        }
        return false;
    }

    out->success = true;
    out->data = nullptr;
    out->size = bytecode->size();
    out->error_message[0] = '\0';

    return true;
}

/// Generate bytecode for an ES module and write to a file
bool otter_generate_module_bytecode_to_file(
    JSContextRef ctx,
    const char* source,
    size_t source_len,
    const char* filename,
    size_t filename_len,
    const char* output_path,
    size_t output_path_len,
    OtterBytecodeResult* out)
{
    if (!ctx || !source || !output_path || !out) {
        if (out) {
            out->success = false;
            snprintf(out->error_message, sizeof(out->error_message), "Invalid arguments");
        }
        return false;
    }

    auto* globalObject = toJS(ctx);
    if (!globalObject) {
        out->success = false;
        snprintf(out->error_message, sizeof(out->error_message), "Invalid context");
        return false;
    }

    JSC::VM& vm = getVM(globalObject);

    // Create source code
    WTF::String sourceString = WTF::String::fromUTF8(source, source_len);
    WTF::String filenameString = filename ? WTF::String::fromUTF8(filename, filename_len) : WTF::String("module.js"_s);
    WTF::String outputPathString = WTF::String::fromUTF8(output_path, output_path_len);

    JSC::SourceCode sourceCode = JSC::makeSource(
        sourceString,
        JSC::SourceOrigin(WTF::URL(filenameString)),
        JSC::SourceTaintedOrigin::Untainted,
        filenameString,
        WTF::TextPosition(),
        JSC::SourceProviderSourceType::Module
    );

    // Open output file
    auto fileHandle = FileSystem::openFile(outputPathString, FileSystem::FileOpenMode::Truncate);
    if (!FileSystem::isHandleValid(fileHandle)) {
        out->success = false;
        snprintf(out->error_message, sizeof(out->error_message), "Failed to open output file: %s", output_path);
        return false;
    }

    // Generate bytecode
    JSC::BytecodeCacheError error;
    RefPtr<JSC::CachedBytecode> bytecode = JSC::generateModuleBytecode(vm, sourceCode, fileHandle, error);

    FileSystem::closeFile(fileHandle);

    if (!bytecode || error.isValid()) {
        out->success = false;
        if (error.isValid()) {
            WTF::String errorMsg = error.message();
            auto utf8 = errorMsg.utf8();
            snprintf(out->error_message, sizeof(out->error_message), "%s", utf8.data());
        } else {
            snprintf(out->error_message, sizeof(out->error_message), "Module bytecode generation failed");
        }
        return false;
    }

    out->success = true;
    out->data = nullptr;
    out->size = bytecode->size();
    out->error_message[0] = '\0';

    return true;
}

} // extern "C"
