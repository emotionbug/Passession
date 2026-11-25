// C stubs for WASM targets
// tree-sitter expects fprintf to be available, but we don't need it

#include <stdarg.h>

// Forward declare FILE to avoid needing full stdio.h
typedef struct _IO_FILE FILE;

int fprintf(FILE* stream, const char* format, ...) {
    // No-op implementation for WASM
    (void)stream;
    (void)format;
    return 0;
}
