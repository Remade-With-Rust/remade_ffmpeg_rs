/* OpenJPEG oracle for rusty_jpx: decode a JPEG 2000 file with libopenjp2 and
 * write its component buffers, untouched (no colour conversion, no
 * upsampling), in the format rusty_jpx's `jpxdump` example writes.
 *
 *   cc -O2 -o opj_dump opj_dump.c $(pkg-config --cflags --libs libopenjp2)
 *   opj_dump [--lenient] IN OUT
 *
 * --lenient turns OpenJPEG's strict mode off (opj_decoder_set_strict_mode),
 * so that it decodes what a truncated codestream still holds.
 *
 * OUT: a text header, then each component's samples as little-endian i32:
 *   JDMP <components> <x0> <y0> <x1> <y1>\n
 *   <w> <h> <x0> <y0> <dx> <dy> <prec> <sgnd>\n   (one line per component)
 * Prints "ok <warnings>" or "err" on stdout. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "openjpeg.h"

static int warnings;
static void on_warning(const char *m, void *u) { (void)u; (void)m; warnings++; }
static void on_error(const char *m, void *u) { (void)u; fprintf(stderr, "opj: %s", m); }

int main(int argc, char **argv) {
    int strict = 1, a = 1;
    if (argc > 1 && !strcmp(argv[1], "--lenient")) { strict = 0; a++; }
    if (argc - a != 2) { fprintf(stderr, "usage: opj_dump [--lenient] IN OUT\n"); return 2; }
    FILE *f = fopen(argv[a], "rb");
    if (!f) { printf("err\n"); return 1; }
    unsigned char head[4] = {0};
    size_t got = fread(head, 1, 4, f);
    fclose(f);
    int j2k = got >= 2 && head[0] == 0xFF && head[1] == 0x4F;
    opj_stream_t *s = opj_stream_create_default_file_stream(argv[a], 1);
    opj_codec_t *codec = opj_create_decompress(j2k ? OPJ_CODEC_J2K : OPJ_CODEC_JP2);
    opj_dparameters_t p;
    opj_set_default_decoder_parameters(&p);
    p.flags |= OPJ_DPARAMETERS_IGNORE_PCLR_CMAP_CDEF_FLAG;
    opj_set_error_handler(codec, on_error, 0);
    opj_set_warning_handler(codec, on_warning, 0);
    opj_setup_decoder(codec, &p);
    opj_decoder_set_strict_mode(codec, strict ? OPJ_TRUE : OPJ_FALSE);
    opj_image_t *im = 0;
    int ok = opj_read_header(s, codec, &im) && opj_set_decode_area(codec, im, 0, 0, 0, 0) &&
             opj_decode(codec, s, im) && opj_end_decompress(codec, s);
    opj_stream_destroy(s);
    opj_destroy_codec(codec);
    if (!ok || !im) { printf("err\n"); if (im) opj_image_destroy(im); return 1; }
    FILE *o = fopen(argv[a + 1], "wb");
    fprintf(o, "JDMP %u %u %u %u %u\n", im->numcomps, im->x0, im->y0, im->x1, im->y1);
    for (unsigned c = 0; c < im->numcomps; c++) {
        opj_image_comp_t *q = &im->comps[c];
        fprintf(o, "%u %u %u %u %u %u %u %u\n", q->w, q->h, q->x0, q->y0, q->dx, q->dy, q->prec, q->sgnd);
    }
    for (unsigned c = 0; c < im->numcomps; c++) {
        opj_image_comp_t *q = &im->comps[c];
        size_t n = (size_t)q->w * q->h;
        for (size_t i = 0; i < n; i++) {
            int v = q->data ? q->data[i] : 0;
            unsigned char b[4] = {(unsigned char)v, (unsigned char)(v >> 8), (unsigned char)(v >> 16), (unsigned char)(v >> 24)};
            fwrite(b, 1, 4, o);
        }
    }
    fclose(o);
    printf("ok %d\n", warnings);
    opj_image_destroy(im);
    return 0;
}
