#include <span>
#define JS_EXPORT_PRIVATE __attribute__((visibility("default")))
#include <wtf/ExportMacros.h>
#include <JavaScriptCore/APICast.h>
#include <JavaScriptCore/JSGlobalObjectInlines.h>
#include <JavaScriptCore/Heap.h>
#include <JavaScriptCore/VM.h>

extern "C" {

struct OtterJscHeapStats {
    size_t heap_size;
    size_t heap_capacity;
    size_t extra_memory;
    size_t array_buffer;
};

bool otter_jsc_heap_stats(JSContextRef ctx, OtterJscHeapStats* out)
{
    if (!ctx || !out)
        return false;

    auto* globalObject = toJS(ctx);
    if (!globalObject)
        return false;

    JSC::VM& vm = getVM(globalObject);
    JSC::Heap& heap = vm.heap;

    out->heap_size = heap.size();
    out->heap_capacity = heap.capacity();
    out->extra_memory = heap.extraMemorySize();
    out->array_buffer = heap.arrayBufferSize();

    return true;
}

} // extern "C"
