// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//
// FR-36 (#929) P1 go/no-go probe — grab the live scanout framebuffer via
// DRM/KMS, below the compositor. NOT built by cargo; this is the research
// probe that produced the FR-36 field measurements, kept so the numbers are
// reproducible and P1 does not start from a blank page.
//
//   gcc -O2 -o drmgrab scripts/drm-capture-probe.c $(pkg-config --cflags --libs libdrm)
//   sudo ./drmgrab /tmp/grab.ppm      # needs CAP_SYS_ADMIN for the GEM handle
//
// Measured on scw-m2-asahi (Fedora Asahi Remix 42, kernel 6.19.14-asahi):
//   X11/XFCE        fb 1920x1080 XR24 modifier=0x0   read 1.58 ms
//   GNOME Wayland   fb 4096x2160 XR30 modifier=0x0   read 15.2 ms
// The apple-drm primary plane advertises ONLY DRM_FORMAT_MOD_LINEAR, so no
// compositor on this hardware can scan out a tiled buffer — which is why
// FR-36 P2 (detiling) is not on the critical path for Apple Silicon.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdint.h>
#include <sys/mman.h>
#include <xf86drm.h>
#include <xf86drmMode.h>
#include <drm_fourcc.h>

static uint64_t plane_prop(int fd, uint32_t obj, uint32_t type, const char *want, int *found) {
    uint64_t val = 0; *found = 0;
    drmModeObjectProperties *props = drmModeObjectGetProperties(fd, obj, type);
    if (!props) return 0;
    for (uint32_t i = 0; i < props->count_props; i++) {
        drmModePropertyRes *p = drmModeGetProperty(fd, props->props[i]);
        if (!p) continue;
        if (!strcmp(p->name, want)) { val = props->prop_values[i]; *found = 1; }
        drmModeFreeProperty(p);
        if (*found) break;
    }
    drmModeFreeObjectProperties(props);
    return val;
}

int main(int argc, char **argv) {
    const char *out = argc > 1 ? argv[1] : "/tmp/drmgrab.ppm";
    int fd = -1; char path[64];

    for (int i = 0; i < 16; i++) {
        snprintf(path, sizeof path, "/dev/dri/card%d", i);
        int t = open(path, O_RDWR | O_CLOEXEC);
        if (t < 0) continue;
        drmModeRes *r = drmModeGetResources(t);
        int is_display = r && r->count_crtcs > 0 && r->count_connectors > 0;
        if (r) drmModeFreeResources(r);
        if (is_display) {
            drmVersionPtr v = drmGetVersion(t);
            fprintf(stderr, "using %s (driver=%s)\n", path, v ? v->name : "?");
            if (v) drmFreeVersion(v);
            fd = t; break;
        }
        close(t);
    }
    if (fd < 0) { fprintf(stderr, "no DRM display node found\n"); return 2; }

    drmSetClientCap(fd, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1);
    drmSetClientCap(fd, DRM_CLIENT_CAP_ATOMIC, 1);

    drmModePlaneRes *pres = drmModeGetPlaneResources(fd);
    if (!pres) { fprintf(stderr, "GetPlaneResources: %s\n", strerror(errno)); return 2; }
    uint32_t fb_id = 0, crtc_id = 0;
    for (uint32_t i = 0; i < pres->count_planes && !fb_id; i++) {
        drmModePlane *pl = drmModeGetPlane(fd, pres->planes[i]);
        if (!pl) continue;
        if (pl->crtc_id && pl->fb_id) {
            int f = 0; uint64_t t = plane_prop(fd, pl->plane_id, DRM_MODE_OBJECT_PLANE, "type", &f);
            if (!f || t == DRM_PLANE_TYPE_PRIMARY) { fb_id = pl->fb_id; crtc_id = pl->crtc_id; }
        }
        drmModeFreePlane(pl);
    }
    drmModeFreePlaneResources(pres);
    if (!fb_id) { fprintf(stderr, "no primary plane with a live framebuffer\n"); return 3; }

    drmModeFB2 *fb = drmModeGetFB2(fd, fb_id);
    if (!fb) { fprintf(stderr, "GetFB2(%u): %s (need CAP_SYS_ADMIN)\n", fb_id, strerror(errno)); return 4; }
    fprintf(stderr, "fb %u crtc %u  %ux%u  fourcc=%.4s  modifier=0x%016llx  pitch=%u\n",
            fb_id, crtc_id, fb->width, fb->height, (char *)&fb->pixel_format,
            (unsigned long long)fb->modifier, fb->pitches[0]);

    if (fb->modifier != DRM_FORMAT_MOD_LINEAR && fb->modifier != DRM_FORMAT_MOD_INVALID) {
        fprintf(stderr, "NON-LINEAR modifier — detiling required, aborting grab\n"); return 5;
    }
    if (!fb->handles[0]) { fprintf(stderr, "no GEM handle (needs CAP_SYS_ADMIN)\n"); return 6; }

    // Pixel unpack depends on the fourcc. XR24/AR24 are 8-bit B,G,R,X bytes;
    // XR30/AR30 are one packed 32-bit LE word, x:R:G:B 2:10:10:10. GNOME picked
    // XR30 on this display, and decoding it as XR24 gives a structurally
    // perfect but psychedelic image — so branch on the format rather than
    // assuming the 8-bit layout every desktop used to hand us.
    int is30 = (fb->pixel_format == DRM_FORMAT_XRGB2101010 || fb->pixel_format == DRM_FORMAT_ARGB2101010);
    int is24 = (fb->pixel_format == DRM_FORMAT_XRGB8888   || fb->pixel_format == DRM_FORMAT_ARGB8888);
    if (!is30 && !is24) { fprintf(stderr, "unhandled fourcc %.4s\n", (char *)&fb->pixel_format); return 10; }

    int dbuf = -1;
    if (drmPrimeHandleToFD(fd, fb->handles[0], DRM_CLOEXEC | DRM_RDWR, &dbuf))
        if (drmPrimeHandleToFD(fd, fb->handles[0], DRM_CLOEXEC, &dbuf)) {
            fprintf(stderr, "PrimeHandleToFD: %s\n", strerror(errno)); return 7;
        }
    size_t len = (size_t)fb->pitches[0] * fb->height;
    void *map = mmap(NULL, len, PROT_READ, MAP_SHARED, dbuf, 0);
    if (map == MAP_FAILED) { fprintf(stderr, "mmap dmabuf: %s\n", strerror(errno)); return 8; }

    FILE *f = fopen(out, "wb");
    if (!f) { fprintf(stderr, "open %s: %s\n", out, strerror(errno)); return 9; }
    fprintf(f, "P6\n%u %u\n255\n", fb->width, fb->height);
    unsigned long long sum = 0; unsigned nonzero = 0;
    unsigned char *row = malloc((size_t)fb->width * 3);
    for (uint32_t y = 0; y < fb->height; y++) {
        const unsigned char *src = (const unsigned char *)map + (size_t)y * fb->pitches[0];
        for (uint32_t x = 0; x < fb->width; x++) {
            if (is30) {
                uint32_t v; memcpy(&v, src + (size_t)x * 4, 4);
                row[x*3+0] = (unsigned char)(((v >> 20) & 0x3FF) >> 2);
                row[x*3+1] = (unsigned char)(((v >> 10) & 0x3FF) >> 2);
                row[x*3+2] = (unsigned char)(( v        & 0x3FF) >> 2);
            } else {
                row[x*3+0] = src[x*4+2]; row[x*3+1] = src[x*4+1]; row[x*3+2] = src[x*4+0];
            }
            sum += row[x*3] + row[x*3+1] + row[x*3+2];
            if (row[x*3] | row[x*3+1] | row[x*3+2]) nonzero++;
        }
        fwrite(row, 1, (size_t)fb->width * 3, f);
    }
    fclose(f); free(row);
    double px = (double)fb->width * fb->height;
    fprintf(stderr, "wrote %s  fmt=%s  mean_luma≈%.1f  nonzero_px=%.1f%%\n",
            out, is30 ? "XR30(10-bit)" : "XR24(8-bit)", sum / (px * 3.0), 100.0 * nonzero / px);
    munmap(map, len); close(dbuf); drmModeFreeFB2(fb); close(fd);
    return 0;
}
