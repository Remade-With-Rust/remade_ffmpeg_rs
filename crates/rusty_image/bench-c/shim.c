// C reference engines for the rusty_image bench: stb_image / stb_image_write,
// the reference qoi.h, ImageMagick 7 (MagickWand, prototypes declared here
// because the dev headers are not installed) and gdk-pixbuf.
#define STB_IMAGE_IMPLEMENTATION
#define STB_IMAGE_WRITE_IMPLEMENTATION
#define QOI_IMPLEMENTATION
#include "stb_image.h"
#include "stb_image_write.h"
#include "qoi.h"
#include <gdk-pixbuf/gdk-pixbuf.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>

// ---- stb ----
int c_stb_load(const unsigned char *d, int n, int want, int *w, int *h, int *ch, unsigned char **out) {
    *out = stbi_load_from_memory(d, n, w, h, ch, want);
    return *out ? 0 : -1;
}
int c_stb_load16(const unsigned char *d, int n, int want, int *w, int *h, int *ch, unsigned short **out) {
    *out = stbi_load_16_from_memory(d, n, w, h, ch, want);
    return *out ? 0 : -1;
}
int c_stb_loadf(const unsigned char *d, int n, int *w, int *h, int *ch, float **out) {
    *out = stbi_loadf_from_memory(d, n, w, h, ch, 0);
    return *out ? 0 : -1;
}
void c_stb_free(void *p) { stbi_image_free(p); }

typedef struct { unsigned char *p; size_t len, cap; } Buf;
static void buf_put(void *ctx, void *data, int size) {
    Buf *b = (Buf *)ctx;
    if (b->len + size > b->cap) {
        b->cap = (b->len + size) * 2;
        b->p = realloc(b->p, b->cap);
    }
    memcpy(b->p + b->len, data, size);
    b->len += size;
}
// kind: 0 bmp, 1 tga (RLE), 2 hdr (float data)
long c_stb_write(int kind, int w, int h, int ch, const void *px, unsigned char **out) {
    Buf b = {0, 0, 0};
    int ok = 0;
    if (kind == 0) ok = stbi_write_bmp_to_func(buf_put, &b, w, h, ch, px);
    if (kind == 1) ok = stbi_write_tga_to_func(buf_put, &b, w, h, ch, px);
    if (kind == 2) ok = stbi_write_hdr_to_func(buf_put, &b, w, h, ch, (const float *)px);
    *out = b.p;
    return ok ? (long)b.len : -1;
}
void c_free(void *p) { free(p); }

// ---- qoi.h ----
int c_qoi_decode(const unsigned char *d, int n, int *w, int *h, int *ch, unsigned char **out) {
    qoi_desc desc;
    *out = qoi_decode(d, n, &desc, 0);
    if (!*out) return -1;
    *w = desc.width; *h = desc.height; *ch = desc.channels;
    return 0;
}
long c_qoi_encode(int w, int h, int ch, const unsigned char *px, unsigned char **out) {
    qoi_desc desc = { (unsigned)w, (unsigned)h, (unsigned char)ch, QOI_SRGB };
    int len = 0;
    *out = qoi_encode(px, &desc, &len);
    return *out ? len : -1;
}

// ---- ImageMagick 7 (MagickWand) ----
typedef struct _MagickWand MagickWand;
typedef enum { MagickFalse = 0, MagickTrue = 1 } MagickBooleanType;
enum { CharPixel = 1, FloatPixel = 3, ShortPixel = 7 };
extern void MagickWandGenesis(void);
extern MagickWand *NewMagickWand(void);
extern MagickWand *DestroyMagickWand(MagickWand *);
extern MagickBooleanType MagickSetFilename(MagickWand *, const char *);
extern MagickBooleanType MagickReadImageBlob(MagickWand *, const void *, const size_t);
extern size_t MagickGetImageWidth(MagickWand *);
extern size_t MagickGetImageHeight(MagickWand *);
extern MagickBooleanType MagickExportImagePixels(MagickWand *, const ssize_t, const ssize_t,
    const size_t, const size_t, const char *, const int, void *);
extern MagickBooleanType MagickConstituteImage(MagickWand *, const size_t, const size_t,
    const char *, const int, const void *);
extern MagickBooleanType MagickSetImageFormat(MagickWand *, const char *);
extern MagickBooleanType MagickSetImageCompression(MagickWand *, const int);
extern unsigned char *MagickGetImageBlob(MagickWand *, size_t *);
extern void *MagickRelinquishMemory(void *);
extern MagickBooleanType MagickSetResourceLimit(const int, const unsigned long long);

void c_magick_init(void) {
    MagickWandGenesis();
    MagickSetResourceLimit(5 /* ThreadResource */, 1); // single thread, like the others
}
// Decode `d` (format forced by `hint`, e.g. "tga:x"); with `map` (e.g. "RGBA")
// also export the pixels (storage: 1 char, 3 float, 7 short) into `out`.
int c_magick_decode(const unsigned char *d, size_t n, const char *hint, const char *map,
                    int storage, int *w, int *h, void **out) {
    MagickWand *m = NewMagickWand();
    if (hint) MagickSetFilename(m, hint);
    if (MagickReadImageBlob(m, d, n) != MagickTrue) { DestroyMagickWand(m); return -1; }
    *w = (int)MagickGetImageWidth(m);
    *h = (int)MagickGetImageHeight(m);
    *out = NULL;
    if (map) {
        size_t per = storage == CharPixel ? 1 : storage == ShortPixel ? 2 : 4;
        size_t len = (size_t)(*w) * (size_t)(*h) * strlen(map) * per;
        *out = malloc(len);
        MagickExportImagePixels(m, 0, 0, *w, *h, map, storage, *out);
    }
    DestroyMagickWand(m);
    return 0;
}
long c_magick_encode(const char *fmt, int w, int h, const char *map, int storage,
                     const void *px, unsigned char **out) {
    MagickWand *m = NewMagickWand();
    if (MagickConstituteImage(m, w, h, map, storage, px) != MagickTrue) { DestroyMagickWand(m); return -1; }
    MagickSetImageFormat(m, fmt);
    size_t len = 0;
    unsigned char *blob = MagickGetImageBlob(m, &len);
    *out = malloc(len);
    memcpy(*out, blob, len);
    MagickRelinquishMemory(blob);
    DestroyMagickWand(m);
    return (long)len;
}

// ---- gdk-pixbuf ----
int c_pixbuf_decode(const unsigned char *d, size_t n, int *w, int *h, int *ch, int *stride,
                    unsigned char **out) {
    GdkPixbufLoader *l = gdk_pixbuf_loader_new();
    GError *err = NULL;
    if (!gdk_pixbuf_loader_write(l, d, n, &err) || !gdk_pixbuf_loader_close(l, &err)) {
        if (err) g_error_free(err);
        g_object_unref(l);
        return -1;
    }
    GdkPixbuf *p = gdk_pixbuf_loader_get_pixbuf(l);
    if (!p) { g_object_unref(l); return -1; }
    *w = gdk_pixbuf_get_width(p);
    *h = gdk_pixbuf_get_height(p);
    *ch = gdk_pixbuf_get_n_channels(p);
    *stride = gdk_pixbuf_get_rowstride(p);
    *out = NULL;
    if (out) {
        size_t row = (size_t)(*w) * (*ch);
        *out = malloc(row * (*h));
        const guchar *src = gdk_pixbuf_read_pixels(p);
        for (int y = 0; y < *h; y++) memcpy(*out + row * y, src + (size_t)(*stride) * y, row);
    }
    g_object_unref(l);
    return 0;
}
