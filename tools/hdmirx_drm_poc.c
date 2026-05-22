// PoC: display RK HDMI RX V4L2 capture buffers directly on a DRM/KMS plane.
//
// Build on target:
//   cc tools/hdmirx_drm_poc.c -o /tmp/hdmirx_drm_poc $(pkg-config --cflags --libs libdrm)
// Run as root while omtencoder is stopped:
//   sudo /tmp/hdmirx_drm_poc /dev/video0 /dev/dri/card0 10

#include <errno.h>
#include <fcntl.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include <drm.h>
#include <drm_fourcc.h>
#include <xf86drm.h>
#include <xf86drmMode.h>
#include <linux/videodev2.h>

#ifndef DRM_FORMAT_BGR888
#define DRM_FORMAT_BGR888 fourcc_code('B', 'G', '2', '4')
#endif

#define BUFFER_COUNT 4

struct buffer {
    void *start;
    size_t length;
    int dma_fd;
    uint32_t drm_handle;
    uint32_t fb_id;
};

static int xioctl(int fd, unsigned long request, void *arg) {
    int r;
    do {
        r = ioctl(fd, request, arg);
    } while (r == -1 && errno == EINTR);
    return r;
}

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static bool plane_supports_format(drmModePlane *plane, uint32_t fmt) {
    for (uint32_t i = 0; i < plane->count_formats; i++) {
        if (plane->formats[i] == fmt) return true;
    }
    return false;
}

static int plane_type(int drm_fd, uint32_t plane_id) {
    drmModeObjectProperties *props =
        drmModeObjectGetProperties(drm_fd, plane_id, DRM_MODE_OBJECT_PLANE);
    if (!props) return -1;
    int result = -1;
    for (uint32_t i = 0; i < props->count_props; i++) {
        drmModePropertyRes *prop = drmModeGetProperty(drm_fd, props->props[i]);
        if (!prop) continue;
        if (strcmp(prop->name, "type") == 0) {
            result = (int)props->prop_values[i];
            drmModeFreeProperty(prop);
            break;
        }
        drmModeFreeProperty(prop);
    }
    drmModeFreeObjectProperties(props);
    return result;
}

static int crtc_index(drmModeRes *res, uint32_t crtc_id) {
    for (int i = 0; i < res->count_crtcs; i++) {
        if ((uint32_t)res->crtcs[i] == crtc_id) return i;
    }
    return -1;
}

static uint32_t find_connected_crtc(int drm_fd, drmModeRes *res, uint32_t *connector_id) {
    for (int i = 0; i < res->count_connectors; i++) {
        drmModeConnector *conn = drmModeGetConnector(drm_fd, res->connectors[i]);
        if (!conn) continue;
        bool connected = conn->connection == DRM_MODE_CONNECTED && conn->count_modes > 0;
        if (!connected) {
            drmModeFreeConnector(conn);
            continue;
        }
        uint32_t crtc_id = 0;
        if (conn->encoder_id) {
            drmModeEncoder *enc = drmModeGetEncoder(drm_fd, conn->encoder_id);
            if (enc) {
                crtc_id = enc->crtc_id;
                drmModeFreeEncoder(enc);
            }
        }
        if (!crtc_id && conn->count_encoders > 0) {
            drmModeEncoder *enc = drmModeGetEncoder(drm_fd, conn->encoders[0]);
            if (enc) {
                for (int c = 0; c < res->count_crtcs; c++) {
                    if (enc->possible_crtcs & (1 << c)) {
                        crtc_id = res->crtcs[c];
                        break;
                    }
                }
                drmModeFreeEncoder(enc);
            }
        }
        if (crtc_id) {
            *connector_id = conn->connector_id;
            drmModeFreeConnector(conn);
            return crtc_id;
        }
        drmModeFreeConnector(conn);
    }
    return 0;
}

static uint32_t find_plane(int drm_fd, drmModeRes *res, uint32_t crtc_id, uint32_t fmt) {
    int cidx = crtc_index(res, crtc_id);
    if (cidx < 0) return 0;

    drmModePlaneRes *planes = drmModeGetPlaneResources(drm_fd);
    if (!planes) return 0;

    uint32_t fallback = 0;
    for (uint32_t i = 0; i < planes->count_planes; i++) {
        drmModePlane *plane = drmModeGetPlane(drm_fd, planes->planes[i]);
        if (!plane) continue;
        bool ok = (plane->possible_crtcs & (1 << cidx)) && plane_supports_format(plane, fmt);
        int type = ok ? plane_type(drm_fd, plane->plane_id) : -1;
        uint32_t id = plane->plane_id;
        drmModeFreePlane(plane);
        if (!ok) continue;
        if (type == DRM_PLANE_TYPE_PRIMARY) {
            drmModeFreePlaneResources(planes);
            return id;
        }
        if (!fallback) fallback = id;
    }

    drmModeFreePlaneResources(planes);
    return fallback;
}

int main(int argc, char **argv) {
    const char *video_path = argc > 1 ? argv[1] : "/dev/video0";
    const char *drm_path = argc > 2 ? argv[2] : "/dev/dri/card0";
    int seconds = argc > 3 ? atoi(argv[3]) : 10;
    if (seconds <= 0) seconds = 10;

    int vfd = open(video_path, O_RDWR | O_NONBLOCK);
    if (vfd < 0) {
        perror("open video");
        return 1;
    }

    int dfd = open(drm_path, O_RDWR | O_CLOEXEC);
    if (dfd < 0) {
        perror("open drm");
        return 1;
    }
    drmSetClientCap(dfd, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1);

    drmModeRes *res = drmModeGetResources(dfd);
    if (!res) {
        perror("drmModeGetResources");
        return 1;
    }
    uint32_t connector_id = 0;
    uint32_t crtc_id = find_connected_crtc(dfd, res, &connector_id);
    uint32_t plane_id = find_plane(dfd, res, crtc_id, DRM_FORMAT_BGR888);
    fprintf(stderr, "DRM connector=%u crtc=%u plane=%u format=BG24\n", connector_id, crtc_id, plane_id);
    if (!connector_id || !crtc_id || !plane_id) return 1;

    struct v4l2_format fmt;
    memset(&fmt, 0, sizeof(fmt));
    fmt.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    fmt.fmt.pix_mp.width = 1920;
    fmt.fmt.pix_mp.height = 1080;
    fmt.fmt.pix_mp.pixelformat = V4L2_PIX_FMT_BGR24;
    fmt.fmt.pix_mp.field = V4L2_FIELD_NONE;
    fmt.fmt.pix_mp.num_planes = 1;
    if (xioctl(vfd, VIDIOC_S_FMT, &fmt) < 0) {
        perror("VIDIOC_S_FMT");
    }
    memset(&fmt, 0, sizeof(fmt));
    fmt.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    if (xioctl(vfd, VIDIOC_G_FMT, &fmt) < 0) {
        perror("VIDIOC_G_FMT");
        return 1;
    }
    uint32_t width = fmt.fmt.pix_mp.width;
    uint32_t height = fmt.fmt.pix_mp.height;
    uint32_t pitch = fmt.fmt.pix_mp.plane_fmt[0].bytesperline;
    fprintf(stderr, "V4L2 %ux%u fourcc=%c%c%c%c pitch=%u size=%u\n",
            width, height,
            fmt.fmt.pix_mp.pixelformat & 0xff,
            (fmt.fmt.pix_mp.pixelformat >> 8) & 0xff,
            (fmt.fmt.pix_mp.pixelformat >> 16) & 0xff,
            (fmt.fmt.pix_mp.pixelformat >> 24) & 0xff,
            pitch, fmt.fmt.pix_mp.plane_fmt[0].sizeimage);

    struct v4l2_requestbuffers req;
    memset(&req, 0, sizeof(req));
    req.count = BUFFER_COUNT;
    req.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    req.memory = V4L2_MEMORY_MMAP;
    if (xioctl(vfd, VIDIOC_REQBUFS, &req) < 0) {
        perror("VIDIOC_REQBUFS");
        return 1;
    }
    if (req.count < 2) {
        fprintf(stderr, "not enough V4L2 buffers\n");
        return 1;
    }

    struct buffer bufs[BUFFER_COUNT];
    memset(bufs, 0, sizeof(bufs));
    for (uint32_t i = 0; i < req.count; i++) {
        struct v4l2_plane planes[VIDEO_MAX_PLANES];
        memset(planes, 0, sizeof(planes));
        struct v4l2_buffer buf;
        memset(&buf, 0, sizeof(buf));
        buf.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        buf.memory = V4L2_MEMORY_MMAP;
        buf.index = i;
        buf.length = VIDEO_MAX_PLANES;
        buf.m.planes = planes;
        if (xioctl(vfd, VIDIOC_QUERYBUF, &buf) < 0) {
            perror("VIDIOC_QUERYBUF");
            return 1;
        }
        bufs[i].length = planes[0].length;
        bufs[i].start = mmap(NULL, planes[0].length, PROT_READ | PROT_WRITE, MAP_SHARED, vfd, planes[0].m.mem_offset);
        if (bufs[i].start == MAP_FAILED) {
            perror("mmap v4l2");
            return 1;
        }

        struct v4l2_exportbuffer exp;
        memset(&exp, 0, sizeof(exp));
        exp.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        exp.index = i;
        exp.plane = 0;
        exp.flags = O_CLOEXEC;
        if (xioctl(vfd, VIDIOC_EXPBUF, &exp) < 0) {
            perror("VIDIOC_EXPBUF");
            return 1;
        }
        bufs[i].dma_fd = exp.fd;
        if (drmPrimeFDToHandle(dfd, bufs[i].dma_fd, &bufs[i].drm_handle) != 0) {
            perror("drmPrimeFDToHandle");
            return 1;
        }
        uint32_t handles[4] = { bufs[i].drm_handle, 0, 0, 0 };
        uint32_t pitches[4] = { pitch, 0, 0, 0 };
        uint32_t offsets[4] = { 0, 0, 0, 0 };
        if (drmModeAddFB2(dfd, width, height, DRM_FORMAT_BGR888, handles, pitches, offsets, &bufs[i].fb_id, 0) != 0) {
            perror("drmModeAddFB2");
            return 1;
        }

        if (xioctl(vfd, VIDIOC_QBUF, &buf) < 0) {
            perror("VIDIOC_QBUF");
            return 1;
        }
    }

    enum v4l2_buf_type type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    if (xioctl(vfd, VIDIOC_STREAMON, &type) < 0) {
        perror("VIDIOC_STREAMON");
        return 1;
    }

    uint64_t start = now_ns();
    uint64_t last = start;
    uint32_t frames = 0;
    uint32_t window = 0;
    while ((now_ns() - start) < (uint64_t)seconds * 1000000000ull) {
        struct v4l2_plane planes[VIDEO_MAX_PLANES];
        memset(planes, 0, sizeof(planes));
        struct v4l2_buffer buf;
        memset(&buf, 0, sizeof(buf));
        buf.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        buf.memory = V4L2_MEMORY_MMAP;
        buf.length = VIDEO_MAX_PLANES;
        buf.m.planes = planes;
        if (xioctl(vfd, VIDIOC_DQBUF, &buf) < 0) {
            if (errno == EAGAIN) {
                usleep(1000);
                continue;
            }
            perror("VIDIOC_DQBUF");
            break;
        }
        if (buf.index < req.count) {
            int r = drmModeSetPlane(dfd, plane_id, crtc_id, bufs[buf.index].fb_id, 0,
                                    0, 0, width, height,
                                    0, 0, width << 16, height << 16);
            if (r != 0) {
                perror("drmModeSetPlane");
                break;
            }
        }
        frames++;
        window++;
        uint64_t n = now_ns();
        if (n - last >= 1000000000ull) {
            fprintf(stderr, "display fps %.1f total=%u\n", (double)window * 1000000000.0 / (double)(n - last), frames);
            window = 0;
            last = n;
        }
        if (xioctl(vfd, VIDIOC_QBUF, &buf) < 0) {
            perror("VIDIOC_QBUF");
            break;
        }
    }

    xioctl(vfd, VIDIOC_STREAMOFF, &type);
    for (uint32_t i = 0; i < req.count; i++) {
        if (bufs[i].fb_id) drmModeRmFB(dfd, bufs[i].fb_id);
        if (bufs[i].drm_handle) {
            struct drm_gem_close close_arg;
            memset(&close_arg, 0, sizeof(close_arg));
            close_arg.handle = bufs[i].drm_handle;
            ioctl(dfd, DRM_IOCTL_GEM_CLOSE, &close_arg);
        }
        if (bufs[i].dma_fd > 0) close(bufs[i].dma_fd);
        if (bufs[i].start && bufs[i].start != MAP_FAILED) munmap(bufs[i].start, bufs[i].length);
    }
    drmModeFreeResources(res);
    close(dfd);
    close(vfd);
    return 0;
}
