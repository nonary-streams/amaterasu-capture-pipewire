#ifndef AMATERASU_PIPEWIRE_ADAPTER_H
#define AMATERASU_PIPEWIRE_ADAPTER_H
#include <stddef.h>
#include <stdint.h>

/* Private ABI only. SPA/PipeWire/GIO structs never cross this boundary. */
enum am_pw_state { AM_PW_SELECTING = 1, AM_PW_READY, AM_PW_DENIED, AM_PW_FAILED, AM_PW_REVOKED };
struct am_pw_probe { uint32_t available, version, source_types, cursor_modes; };
enum am_pw_stage {
    AM_PW_STAGE_CONNECTING_SESSION_BUS = 1,
    AM_PW_STAGE_CREATE_SESSION,
    AM_PW_STAGE_SELECT_SOURCES,
    AM_PW_STAGE_START,
    AM_PW_STAGE_OPEN_PIPEWIRE_REMOTE,
    AM_PW_STAGE_CONNECT_PIPEWIRE,
    AM_PW_STAGE_WAITING_FOR_FRAMES,
    AM_PW_STAGE_FRAME_RECEIVED,
};
struct am_pw_snapshot {
    uint32_t state, stage, node, width, height, format, error;
    int32_t pipewire_state;
    uint32_t node_present, serial_present;
    int32_t pipewire_error;
    uint64_t serial;
    int32_t left, top;
    uint32_t logical_width, logical_height;
    int32_t desktop_left, desktop_top;
    uint32_t desktop_width, desktop_height;
};
struct am_pw_plane {
    const uint8_t *data;
    uint32_t memory_type, flags, max_size, chunk_offset, chunk_size;
    int32_t stride;
    uint32_t chunk_flags, plane_count;
    int64_t fd;
    uint32_t map_offset;
};
/* 1: a valid owned copy was published; 0: transient mailbox backpressure;
 * 2: unsupported/invalid CPU buffer. Borrowed plane bytes last only through call. */
typedef uint32_t (*am_pw_frame_callback)(void *, const struct am_pw_plane *, uint32_t, uint32_t, uint32_t, uint64_t);
typedef void (*am_pw_token_callback)(void *, const char *);
struct am_pw_session;
int am_pw_probe(struct am_pw_probe *);
/* File containment only; never maps pixels or closes this borrowed FD. */
int am_pw_memfd_extent_valid(int64_t fd, uint32_t map_offset, uint32_t max_size);
/* restore_token is copied before return; callback tokens are borrowed only during
 * the call. Both callbacks share context, retained until destroy joins the worker. */
struct am_pw_session *am_pw_start(uint32_t source_type, uint32_t timeout_seconds, const char *restore_token, uint32_t persist, am_pw_token_callback, am_pw_frame_callback, void *);
void am_pw_snapshot(struct am_pw_session *, struct am_pw_snapshot *);
/* Cancels synchronous D-Bus I/O, closes request/session and joins the sole worker.
 * No callback or adapter worker can outlive this return. */
void am_pw_destroy(struct am_pw_session *);
#endif
