#include <stdbool.h>
#include <stddef.h>

typedef const struct OpaqueJSContext* JSContextRef;

struct OtterJscHeapStats {
    size_t heap_size;
    size_t heap_capacity;
    size_t extra_memory;
    size_t array_buffer;
};

bool otter_jsc_heap_stats(JSContextRef ctx, struct OtterJscHeapStats* out)
{
    (void)ctx;
    if (!out)
        return false;

    out->heap_size = 0;
    out->heap_capacity = 0;
    out->extra_memory = 0;
    out->array_buffer = 0;
    return false;
}
