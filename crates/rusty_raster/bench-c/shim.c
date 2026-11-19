// C references for the rusty_raster / rusty_svgtree bench: Cairo (2D
// rasteriser) and nanosvg + nanosvgrast (SVG).
#include <cairo.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#define NANOSVG_IMPLEMENTATION
#include "nanosvg.h"
#define NANOSVGRAST_IMPLEMENTATION
#include "nanosvgrast.h"

typedef struct { cairo_surface_t *s; cairo_t *cr; } Ctx;

void *cr_new(int w, int h) {
    Ctx *c = malloc(sizeof *c);
    c->s = cairo_image_surface_create(CAIRO_FORMAT_ARGB32, w, h);
    c->cr = cairo_create(c->s);
    return c;
}
void cr_free(void *p) {
    Ctx *c = p;
    cairo_destroy(c->cr);
    cairo_surface_destroy(c->s);
    free(c);
}
void cr_clear(void *p) {
    Ctx *c = p;
    cairo_save(c->cr);
    cairo_set_operator(c->cr, CAIRO_OPERATOR_CLEAR);
    cairo_paint(c->cr);
    cairo_restore(c->cr);
}

// ops: 0 move, 1 line, 2 quad, 3 cubic, 4 close. paint: kind 0 solid
// (r,g,b,a), 1 linear (x0,y0,x1,y1, r0,g0,b0,a0, r1,g1,b1,a1), 2 radial
// (cx,cy,r, same stops). stroke: width<=0 means fill.
void cr_draw(void *p, const unsigned char *ops, int nops, const float *pts, int even_odd,
             float width, int cap, int join, float miter, const float *dash, int ndash,
             int kind, const float *pp, float tx, float ty) {
    Ctx *c = p;
    cairo_t *cr = c->cr;
    cairo_save(cr);
    cairo_translate(cr, tx, ty);
    cairo_new_path(cr);
    double lx = 0, ly = 0;
    const float *q = pts;
    for (int i = 0; i < nops; i++) {
        switch (ops[i]) {
        case 0: cairo_move_to(cr, q[0], q[1]); lx = q[0]; ly = q[1]; q += 2; break;
        case 1: cairo_line_to(cr, q[0], q[1]); lx = q[0]; ly = q[1]; q += 2; break;
        case 2: {
            double cx = q[0], cy = q[1], x = q[2], y = q[3];
            cairo_curve_to(cr, lx + 2.0 / 3.0 * (cx - lx), ly + 2.0 / 3.0 * (cy - ly),
                           x + 2.0 / 3.0 * (cx - x), y + 2.0 / 3.0 * (cy - y), x, y);
            lx = x; ly = y; q += 4; break;
        }
        case 3: cairo_curve_to(cr, q[0], q[1], q[2], q[3], q[4], q[5]); lx = q[4]; ly = q[5]; q += 6; break;
        case 4: cairo_close_path(cr); break;
        }
    }
    cairo_pattern_t *pat = NULL;
    if (kind == 0) {
        cairo_set_source_rgba(cr, pp[0], pp[1], pp[2], pp[3]);
    } else {
        const float *s = kind == 1 ? pp + 4 : pp + 3;
        if (kind == 1) pat = cairo_pattern_create_linear(pp[0], pp[1], pp[2], pp[3]);
        else pat = cairo_pattern_create_radial(pp[0], pp[1], 0, pp[0], pp[1], pp[2]);
        cairo_pattern_add_color_stop_rgba(pat, 0, s[0], s[1], s[2], s[3]);
        cairo_pattern_add_color_stop_rgba(pat, 1, s[4], s[5], s[6], s[7]);
        cairo_set_source(cr, pat);
    }
    if (width > 0) {
        cairo_set_line_width(cr, width);
        cairo_set_line_cap(cr, (cairo_line_cap_t)cap);
        cairo_set_line_join(cr, (cairo_line_join_t)join);
        cairo_set_miter_limit(cr, miter);
        if (ndash > 0) {
            double d[16];
            for (int i = 0; i < ndash && i < 16; i++) d[i] = dash[i];
            cairo_set_dash(cr, d, ndash, 0);
        }
        cairo_stroke(cr);
    } else {
        cairo_set_fill_rule(cr, even_odd ? CAIRO_FILL_RULE_EVEN_ODD : CAIRO_FILL_RULE_WINDING);
        cairo_fill(cr);
    }
    if (pat) cairo_pattern_destroy(pat);
    cairo_restore(cr);
}

// Premultiplied RGBA8 out (Cairo stores premultiplied ARGB32, native-endian).
void cr_read(void *p, unsigned char *out) {
    Ctx *c = p;
    cairo_surface_flush(c->s);
    int w = cairo_image_surface_get_width(c->s), h = cairo_image_surface_get_height(c->s);
    int stride = cairo_image_surface_get_stride(c->s);
    const unsigned char *d = cairo_image_surface_get_data(c->s);
    for (int y = 0; y < h; y++) {
        const unsigned int *row = (const unsigned int *)(d + (size_t)stride * y);
        for (int x = 0; x < w; x++) {
            unsigned int v = row[x];
            unsigned char *o = out + ((size_t)y * w + x) * 4;
            o[0] = (v >> 16) & 255; o[1] = (v >> 8) & 255; o[2] = v & 255; o[3] = v >> 24;
        }
    }
}

// nanosvg: parse and rasterise `svg` scaled by `scale` into straight-alpha
// RGBA8 `out` (w x h). Returns 0 on success.
int nsvg_render(const char *svg, size_t len, float scale, int w, int h, unsigned char *out) {
    char *buf = malloc(len + 1);
    memcpy(buf, svg, len);
    buf[len] = 0;
    NSVGimage *img = nsvgParse(buf, "px", 96.0f);
    free(buf);
    if (!img) return -1;
    NSVGrasterizer *r = nsvgCreateRasterizer();
    nsvgRasterize(r, img, 0, 0, scale, out, w, h, w * 4);
    nsvgDeleteRasterizer(r);
    nsvgDelete(img);
    return 0;
}
// The size nanosvg parses (for checking it read the same document).
int nsvg_size(const char *svg, size_t len, float *w, float *h) {
    char *buf = malloc(len + 1);
    memcpy(buf, svg, len);
    buf[len] = 0;
    NSVGimage *img = nsvgParse(buf, "px", 96.0f);
    free(buf);
    if (!img) return -1;
    *w = img->width; *h = img->height;
    nsvgDelete(img);
    return 0;
}
