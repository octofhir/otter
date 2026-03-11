// Test library for otter:ffi integration tests
#include <string.h>
#include <stdlib.h>

int add(int a, int b) {
    return a + b;
}

double multiply(double a, double b) {
    return a * b;
}

const char* hello(void) {
    return "Hello from C!";
}

int negate(int x) {
    return -x;
}

unsigned int square(unsigned int x) {
    return x * x;
}

float add_float(float a, float b) {
    return a + b;
}

int is_positive(int x) {
    return x > 0 ? 1 : 0;
}

void do_nothing(void) {
    // No-op
}

// Callback test: apply a function to two ints and return the result
typedef int (*int_binop)(int, int);

int apply_binop(int a, int b, int_binop fn) {
    return fn(a, b);
}

// Callback test: transform array elements via callback
typedef int (*int_transform)(int);

void transform_array(int* arr, int len, int_transform fn) {
    for (int i = 0; i < len; i++) {
        arr[i] = fn(arr[i]);
    }
}
