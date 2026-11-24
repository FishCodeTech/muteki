// gpui's core-video dependency pulls io-surface, which compiles its
// macOS-only CGL texture binding for iOS as well. Nothing on iOS calls it,
// but the static library is force-loaded whole, so the linker needs these
// three symbols defined. They are never reached.
#include <stdint.h>

const char *CGLErrorString(int32_t error) {
    (void)error;
    return "CGL is unavailable on iOS";
}

void *CGLGetCurrentContext(void) {
    return 0;
}

int32_t CGLTexImageIOSurface2D(void *ctx, uint32_t target, uint32_t internal_format,
                               int32_t width, int32_t height, uint32_t format,
                               uint32_t type, void *io_surface, uint32_t plane) {
    (void)ctx; (void)target; (void)internal_format; (void)width; (void)height;
    (void)format; (void)type; (void)io_surface; (void)plane;
    return 10000; // kCGLBadAttribute: no context exists here
}
