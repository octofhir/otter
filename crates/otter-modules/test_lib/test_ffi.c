// Test library for otter:ffi integration tests
#include <stdlib.h>
#include <string.h>

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
}

typedef int (*int_binop)(int, int);
typedef int (*int_transform)(int);

int apply_binop(int a, int b, int_binop fn) {
    return fn(a, b);
}

void transform_array(int* arr, int len, int_transform fn) {
    for (int i = 0; i < len; i++) {
        arr[i] = fn(arr[i]);
    }
}

void* add_ptr(void) {
    return (void*)add;
}

void* negate_ptr(void) {
    return (void*)negate;
}
