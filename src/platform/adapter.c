#define _POSIX_C_SOURCE 200809L
#include "adapter.h"
#include "kwin.h"
#include <gio/gio.h>
#include <gio/gunixfdlist.h>
#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>
#include <spa/param/buffers.h>
#include <spa/buffer/meta.h>
#include <stdatomic.h>
#include <limits.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define PORTAL "org.freedesktop.portal.Desktop"
#define PORTAL_PATH "/org/freedesktop/portal/desktop"
#define SCREENCAST "org.freedesktop.portal.ScreenCast"
#define DBUS_TIMEOUT_MS 1000

struct am_pw_session {
    GThread *thread;
    char *named_output;
    GMutex geometry_lock;
    struct am_kwin_geometry geometry;
#ifdef AMATERASU_KWIN
    struct am_kwin *kwin;
#endif
    GCancellable *cancel;
    GMainContext *main_context;
    GDBusConnection *bus;
    char *session_path;
    char *request_path;
    guint closed_subscription;
    atomic_bool stopped;
    atomic_uint state, stage, node, width, height, format, error, node_present, serial_present;
    atomic_int pipewire_state, pipewire_error;
    uint32_t source_type, timeout_seconds;
    atomic_uint_fast64_t serial;
    struct pw_main_loop *loop;
    struct pw_context *context;
    struct pw_core *core;
    struct pw_stream *stream;
    struct spa_hook stream_listener;
    struct spa_hook core_listener;
    am_pw_frame_callback callback;
    am_pw_token_callback token_callback;
    char *restore_token;
    uint32_t persist;
    void *callback_context;
};

struct portal_response { gboolean received; guint32 code; GVariant *results; };

/* GDBus's synchronous authentication has no timeout argument. A joined timer
 * cancels its I/O at the deadline; neither timer nor worker survives module Drop. */
struct connection_deadline { GMutex lock; GCond changed; gboolean done; GCancellable *cancel; };
static gpointer cancel_connection(gpointer data) {
    struct connection_deadline *deadline = data;
    g_mutex_lock(&deadline->lock);
    gint64 until = g_get_monotonic_time() + DBUS_TIMEOUT_MS * 1000;
    while (!deadline->done && g_cond_wait_until(&deadline->changed, &deadline->lock, until)) {}
    if (!deadline->done) g_cancellable_cancel(deadline->cancel);
    g_mutex_unlock(&deadline->lock);
    return NULL;
}

static GDBusConnection *connect_bus(GCancellable *cancel) {
    const char *address = g_getenv("DBUS_SESSION_BUS_ADDRESS");
    if (!address || !*address) return NULL;
    GCancellable *operation = cancel ? g_object_ref(cancel) : g_cancellable_new();
    struct connection_deadline deadline = { .done = FALSE, .cancel = operation };
    g_mutex_init(&deadline.lock); g_cond_init(&deadline.changed);
    GThread *timer = g_thread_try_new("amaterasu-bus-deadline", cancel_connection, &deadline, NULL);
    if (!timer) { g_object_unref(operation); g_cond_clear(&deadline.changed); g_mutex_clear(&deadline.lock); return NULL; }
    GDBusConnection *connection = g_dbus_connection_new_for_address_sync(address,
        G_DBUS_CONNECTION_FLAGS_AUTHENTICATION_CLIENT | G_DBUS_CONNECTION_FLAGS_MESSAGE_BUS_CONNECTION,
        NULL, operation, NULL);
    g_mutex_lock(&deadline.lock); deadline.done = TRUE; g_cond_signal(&deadline.changed); g_mutex_unlock(&deadline.lock);
    g_thread_join(timer);
    g_cond_clear(&deadline.changed); g_mutex_clear(&deadline.lock);
    g_object_unref(operation);
    return connection;
}

int am_pw_probe(struct am_pw_probe *output) {
    if (!output) return -1;
    memset(output, 0, sizeof(*output));
    GDBusConnection *bus = connect_bus(NULL);
    if (!bus) return -1;
    GVariant *reply = g_dbus_connection_call_sync(bus, PORTAL, PORTAL_PATH,
        "org.freedesktop.DBus.Properties", "GetAll", g_variant_new("(s)", SCREENCAST),
        G_VARIANT_TYPE("(a{sv})"), G_DBUS_CALL_FLAGS_NONE, DBUS_TIMEOUT_MS, NULL, NULL);
    if (reply) {
        GVariant *properties;
        g_variant_get(reply, "(@a{sv})", &properties);
        g_variant_lookup(properties, "version", "u", &output->version);
        g_variant_lookup(properties, "AvailableSourceTypes", "u", &output->source_types);
        g_variant_lookup(properties, "AvailableCursorModes", "u", &output->cursor_modes);
        output->available = output->version >= 2 && (output->source_types & 3) != 0 && (output->cursor_modes & 2) != 0;
        g_variant_unref(properties);
        g_variant_unref(reply);
    }
    g_object_unref(bus);
    return output->available ? 0 : -1;
}

static void fail(struct am_pw_session *session, uint32_t error) {
    atomic_store(&session->error, error);
    atomic_store(&session->state, AM_PW_FAILED);
}

static void response_signal(GDBusConnection *bus, const gchar *sender,
    const gchar *path, const gchar *interface, const gchar *signal,
    GVariant *parameters, gpointer data) {
    (void)bus; (void)sender; (void)path; (void)interface; (void)signal;
    struct portal_response *response = data;
    if (!response->received) {
        g_variant_get(parameters, "(u@a{sv})", &response->code, &response->results);
        response->received = TRUE;
    }
}

static void closed_signal(GDBusConnection *bus, const gchar *sender,
    const gchar *path, const gchar *interface, const gchar *signal,
    GVariant *parameters, gpointer data) {
    (void)bus; (void)sender; (void)path; (void)interface; (void)signal; (void)parameters;
    struct am_pw_session *session = data;
    atomic_store(&session->state, AM_PW_REVOKED);
    atomic_store(&session->stopped, true);
}

static char *object_path(struct am_pw_session *session, const char *kind, const char *token) {
    const char *unique = g_dbus_connection_get_unique_name(session->bus);
    if (!unique || unique[0] != ':') return NULL;
    char *sender = g_strdup(unique + 1);
    for (char *p = sender; *p; ++p) if (*p == '.') *p = '_';
    char *result = g_strdup_printf(PORTAL_PATH "/%s/%s/%s", kind, sender, token);
    g_free(sender);
    return result;
}

static void close_object(struct am_pw_session *session, const char *path, const char *interface) {
    if (!session->bus || !path) return;
    GVariant *reply = g_dbus_connection_call_sync(session->bus, PORTAL, path, interface,
        "Close", NULL, NULL, G_DBUS_CALL_FLAGS_NONE, 500, NULL, NULL);
    if (reply) g_variant_unref(reply);
}

static GVariant *portal_request(struct am_pw_session *session, const char *method,
    const char *token, GVariant *parameters, gint64 deadline) {
    struct portal_response response = {0};
    session->request_path = object_path(session, "request", token);
    if (!session->request_path) return NULL;
    guint subscription = g_dbus_connection_signal_subscribe(session->bus, PORTAL,
        "org.freedesktop.portal.Request", "Response", session->request_path,
        NULL, G_DBUS_SIGNAL_FLAGS_NONE, response_signal, &response, NULL);
    GVariant *reply = g_dbus_connection_call_sync(session->bus, PORTAL, PORTAL_PATH,
        SCREENCAST, method, parameters, G_VARIANT_TYPE("(o)"), G_DBUS_CALL_FLAGS_NONE,
        DBUS_TIMEOUT_MS, session->cancel, NULL);
    if (reply) {
        const char *actual;
        g_variant_get(reply, "(&o)", &actual);
        if (strcmp(actual, session->request_path) != 0) {
            close_object(session, actual, "org.freedesktop.portal.Request");
            fail(session, 2);
        } else {
            while (!response.received && !atomic_load(&session->stopped)
                && g_get_monotonic_time() < deadline) {
                while (g_main_context_iteration(session->main_context, FALSE)) {}
                if (!response.received) g_usleep(10000);
            }
        }
        g_variant_unref(reply);
    }
    g_dbus_connection_signal_unsubscribe(session->bus, subscription);
    if (!response.received) close_object(session, session->request_path, "org.freedesktop.portal.Request");
    g_clear_pointer(&session->request_path, g_free);
    if (!response.received || response.code != 0) {
        if (response.received && response.code == 1) atomic_store(&session->state, AM_PW_DENIED);
        else if (!atomic_load(&session->stopped)) fail(session, 2);
        if (response.results) g_variant_unref(response.results);
        return NULL;
    }
    return response.results;
}

static GVariant *options(const char *token) {
    GVariantBuilder builder;
    g_variant_builder_init(&builder, G_VARIANT_TYPE_VARDICT);
    g_variant_builder_add(&builder, "{sv}", "handle_token", g_variant_new_string(token));
    return g_variant_builder_end(&builder);
}

static gboolean select_portal_source(struct am_pw_session *session, gint64 deadline) {
    char *uuid = g_uuid_string_random();
    for (char *p = uuid; *p; ++p) if (*p == '-') *p = '_';
    char *create_token = g_strdup_printf("amaterasu_create_%s", uuid);
    char *session_token = g_strdup_printf("amaterasu_session_%s", uuid);
    session->session_path = object_path(session, "session", session_token);
    GVariantBuilder builder;
    g_variant_builder_init(&builder, G_VARIANT_TYPE_VARDICT);
    g_variant_builder_add(&builder, "{sv}", "handle_token", g_variant_new_string(create_token));
    g_variant_builder_add(&builder, "{sv}", "session_handle_token", g_variant_new_string(session_token));
    atomic_store(&session->stage, AM_PW_STAGE_CREATE_SESSION);
    GVariant *result = portal_request(session, "CreateSession", create_token,
        g_variant_new("(@a{sv})", g_variant_builder_end(&builder)), deadline);
    g_free(create_token); g_free(session_token);
    if (!result) { g_free(uuid); return FALSE; }
    const char *actual;
    gboolean has_session = g_variant_lookup(result, "session_handle", "&s", &actual);
    gboolean valid = has_session && session->session_path && strcmp(actual, session->session_path) == 0;
    if (!valid && has_session && g_variant_is_object_path(actual))
        close_object(session, actual, "org.freedesktop.portal.Session");
    g_variant_unref(result);
    if (!valid) { fail(session, 2); g_free(uuid); return FALSE; }
    session->closed_subscription = g_dbus_connection_signal_subscribe(session->bus, PORTAL,
        "org.freedesktop.portal.Session", "Closed", session->session_path,
        NULL, G_DBUS_SIGNAL_FLAGS_NONE, closed_signal, session, NULL);
    char *select_token = g_strdup_printf("amaterasu_select_%s", uuid);
    g_variant_builder_init(&builder, G_VARIANT_TYPE_VARDICT);
    g_variant_builder_add(&builder, "{sv}", "handle_token", g_variant_new_string(select_token));
    g_variant_builder_add(&builder, "{sv}", "types", g_variant_new_uint32(session->source_type));
    g_variant_builder_add(&builder, "{sv}", "multiple", g_variant_new_boolean(FALSE));
    /* Embed the compositor cursor: this CPU provider has no cursor-metadata plane. */
    g_variant_builder_add(&builder, "{sv}", "cursor_mode", g_variant_new_uint32(2));
    if (session->persist) {
        g_variant_builder_add(&builder, "{sv}", "persist_mode", g_variant_new_uint32(2));
        if (session->restore_token)
            g_variant_builder_add(&builder, "{sv}", "restore_token", g_variant_new_string(session->restore_token));
    }
    atomic_store(&session->stage, AM_PW_STAGE_SELECT_SOURCES);
    result = portal_request(session, "SelectSources", select_token,
        g_variant_new("(o@a{sv})", session->session_path, g_variant_builder_end(&builder)), deadline);
    g_free(select_token);
    if (!result) { g_free(uuid); return FALSE; }
    g_variant_unref(result);
    char *start_token = g_strdup_printf("amaterasu_start_%s", uuid);
    atomic_store(&session->stage, AM_PW_STAGE_START);
    result = portal_request(session, "Start", start_token,
        g_variant_new("(os@a{sv})", session->session_path, "", options(start_token)), deadline);
    g_free(start_token); g_free(uuid);
    if (!result) return FALSE;
    const char *restore_token = NULL;
    if (session->persist && g_variant_lookup(result, "restore_token", "&s", &restore_token)
        && strlen(restore_token) <= 4096 && session->token_callback)
        session->token_callback(session->callback_context, restore_token);
    GVariant *streams = g_variant_lookup_value(result, "streams", G_VARIANT_TYPE("a(ua{sv})"));
    valid = streams && g_variant_n_children(streams) == 1;
    if (valid) {
        guint32 node;
        GVariant *properties;
        GVariant *stream = g_variant_get_child_value(streams, 0);
        g_variant_get(stream, "(u@a{sv})", &node, &properties);
        atomic_store(&session->node, node);
        atomic_store(&session->node_present, 1);
        uint64_t serial = 0;
        if (g_variant_lookup(properties, "pipewire-serial", "t", &serial)) {
            atomic_store(&session->serial, serial);
            atomic_store(&session->serial_present, 1);
        }
        g_variant_unref(properties); g_variant_unref(stream);
        valid = node != PW_ID_ANY;
    }
    if (streams) g_variant_unref(streams);
    g_variant_unref(result);
    if (!valid) fail(session, 3);
    return valid;
}

static void stream_state_changed(void *data, enum pw_stream_state previous,
    enum pw_stream_state state, const char *error) {
    (void)previous; (void)error;
    struct am_pw_session *session = data;
    atomic_store(&session->pipewire_state, (int)state);
    if (state == PW_STREAM_STATE_ERROR || state == PW_STREAM_STATE_UNCONNECTED) {
        if (state == PW_STREAM_STATE_ERROR) atomic_store(&session->pipewire_error, -1);
        fail(session, 4);
    }
}

static void core_error(void *data, uint32_t id, int sequence, int result, const char *message) {
    (void)id; (void)sequence; (void)message;
    struct am_pw_session *session = data;
    atomic_store(&session->pipewire_error, result);
    fail(session, 4);
}

static void format_changed(void *data, uint32_t id, const struct spa_pod *parameter) {
    struct am_pw_session *session = data;
    if (id != SPA_PARAM_Format) return;
    if (!parameter) {
        if (atomic_load(&session->width)) fail(session, 6);
        return;
    }
    struct spa_video_info_raw information = {0};
    uint32_t media_type = 0, media_subtype = 0;
    if (spa_format_parse(parameter, &media_type, &media_subtype) < 0
        || media_type != SPA_MEDIA_TYPE_video || media_subtype != SPA_MEDIA_SUBTYPE_raw
        || spa_format_video_raw_parse(parameter, &information) < 0
        || (information.format != SPA_VIDEO_FORMAT_BGRA && information.format != SPA_VIDEO_FORMAT_BGRx)
        || information.size.width == 0 || information.size.height == 0
        || information.size.width > 8192 || information.size.height > 8192) {
        fail(session, 5); return;
    }
    uint32_t width = atomic_load(&session->width), height = atomic_load(&session->height);
    uint32_t format = information.format == SPA_VIDEO_FORMAT_BGRA ? 1 : 2;
    if ((width && width != information.size.width) || (height && height != information.size.height)
        || (atomic_load(&session->state) == AM_PW_READY && atomic_load(&session->format) != format)) {
        fail(session, 6); return;
    }
    atomic_store(&session->width, information.size.width);
    atomic_store(&session->height, information.size.height);
    atomic_store(&session->format, format);
    g_printerr("Amaterasu PipeWire negotiated %ux%u, rate %u/%u, maximum %u/%u\n",
        information.size.width, information.size.height,
        information.framerate.num, information.framerate.denom,
        information.max_framerate.num, information.max_framerate.denom);
    uint8_t storage[512];
    struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    const struct spa_pod *parameters[2];
    parameters[0] = spa_pod_builder_add_object(&builder, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
        SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int((1 << SPA_DATA_MemPtr) | (1 << SPA_DATA_MemFd)));
    parameters[1] = spa_pod_builder_add_object(&builder, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
        SPA_PARAM_META_type, SPA_POD_Id(SPA_META_Header),
        SPA_PARAM_META_size, SPA_POD_Int(sizeof(struct spa_meta_header)));
    if (pw_stream_update_params(session->stream, parameters, 2) < 0) fail(session, 5);
}

/* MAP_BUFFERS supplies a pointer at mapoffset, but mmap alone does not prove
 * that reading its full extent cannot pass EOF. The provider must retain the
 * file extent while this dequeued buffer is borrowed; this never owns its FD. */
int am_pw_memfd_extent_valid(int64_t fd, uint32_t map_offset, uint32_t max_size) {
    if (fd < 0 || fd > INT_MAX || max_size == 0) return 0;
    struct stat metadata;
    if (fstat((int)fd, &metadata) < 0 || metadata.st_size < 0) return 0;
    uint64_t end = (uint64_t)map_offset + (uint64_t)max_size;
    return end <= (uint64_t)metadata.st_size;
}

static void process_frame(void *data) {
    struct am_pw_session *session = data;
    struct pw_buffer *buffer, *latest = NULL;
    while ((buffer = pw_stream_dequeue_buffer(session->stream))) {
        if (latest) pw_stream_queue_buffer(session->stream, latest);
        latest = buffer;
    }
    if (!latest) return;
    uint32_t state = atomic_load(&session->state);
    if (atomic_load(&session->stopped) || (state != AM_PW_SELECTING && state != AM_PW_READY)) {
        pw_stream_queue_buffer(session->stream, latest);
        return;
    }
    struct spa_buffer *spa = latest->buffer;
    uint32_t width = atomic_load(&session->width), height = atomic_load(&session->height);
    if (spa && spa->n_datas == 1 && spa->datas && width && height) {
        struct spa_data *plane = &spa->datas[0];
        struct spa_meta_header *header = spa_buffer_find_meta_data(spa, SPA_META_Header, sizeof(*header));
        if (plane->type == SPA_DATA_MemFd &&
            !am_pw_memfd_extent_valid(plane->fd, plane->mapoffset, plane->maxsize)) {
            fail(session, 5);
        } else if (plane->chunk && (!header || !(header->flags & (SPA_META_HEADER_FLAG_CORRUPTED | SPA_META_HEADER_FLAG_GAP)))) {
            struct am_pw_plane descriptor = { .data = plane->data, .memory_type = plane->type,
                .flags = plane->flags, .max_size = plane->maxsize, .chunk_offset = plane->chunk->offset,
                .chunk_size = plane->chunk->size, .stride = plane->chunk->stride,
                .chunk_flags = plane->chunk->flags, .plane_count = spa->n_datas, .fd = plane->fd,
                .map_offset = plane->mapoffset };
            uint64_t timestamp = header && header->pts >= 0 ? (uint64_t)header->pts / 1000
                : (uint64_t)g_get_monotonic_time();
            uint32_t copied = session->callback(session->callback_context, &descriptor,
                width, height, atomic_load(&session->format), timestamp);
            if (copied == 1) {
                uint32_t expected = AM_PW_SELECTING;
                atomic_compare_exchange_strong(&session->state, &expected, AM_PW_READY);
                atomic_store(&session->stage, AM_PW_STAGE_FRAME_RECEIVED);
            }
            else if (copied == 2) fail(session, 5);
        }
    } else {
        fail(session, 5);
    }
    /* Every dequeued buffer is returned exactly once, including invalid planes. */
    pw_stream_queue_buffer(session->stream, latest);
}

static const struct pw_stream_events stream_events = {
    PW_VERSION_STREAM_EVENTS, .state_changed = stream_state_changed,
    .param_changed = format_changed, .process = process_frame,
};
static const struct pw_core_events core_events = { PW_VERSION_CORE_EVENTS, .error = core_error };

static int portal_remote(struct am_pw_session *session) {
    atomic_store(&session->stage, AM_PW_STAGE_OPEN_PIPEWIRE_REMOTE);
    GUnixFDList *descriptors = NULL;
    GVariant *reply = g_dbus_connection_call_with_unix_fd_list_sync(session->bus, PORTAL,
        PORTAL_PATH, SCREENCAST, "OpenPipeWireRemote",
        g_variant_new("(o@a{sv})", session->session_path, options("amaterasu_remote")),
        G_VARIANT_TYPE("(h)"), G_DBUS_CALL_FLAGS_NONE, DBUS_TIMEOUT_MS, NULL,
        &descriptors, session->cancel, NULL);
    if (!reply) return -1;
    gint index; g_variant_get(reply, "(h)", &index);
    int fd = descriptors ? g_unix_fd_list_get(descriptors, index, NULL) : -1;
    if (descriptors) g_object_unref(descriptors);
    g_variant_unref(reply);
    return fd;
}

static gboolean connect_pipewire(struct am_pw_session *session, int fd) {
    if (fd < 0 && !session->named_output) return FALSE;
    atomic_store(&session->stage, AM_PW_STAGE_CONNECT_PIPEWIRE);
    session->loop = pw_main_loop_new(NULL);
    if (!session->loop) { close(fd); return FALSE; }
    session->context = pw_context_new(pw_main_loop_get_loop(session->loop), NULL, 0);
    if (!session->context) { close(fd); return FALSE; }
    /* connect_fd transfers fd ownership to PipeWire, including its failure path. */
    session->core = fd >= 0 ? pw_context_connect_fd(session->context, fd, NULL, 0)
        : pw_context_connect(session->context, NULL, 0);
    if (!session->core) return FALSE;
    pw_core_add_listener(session->core, &session->core_listener, &core_events, session);
    struct pw_properties *properties = pw_properties_new(PW_KEY_MEDIA_TYPE, "Video",
        PW_KEY_MEDIA_CATEGORY, "Capture", PW_KEY_MEDIA_ROLE, "Screen",
        PW_KEY_APP_NAME, "Amaterasu", NULL);
    uint64_t serial_value = atomic_load(&session->serial);
    if (serial_value) {
        char serial[32]; snprintf(serial, sizeof(serial), "%llu", (unsigned long long)serial_value);
        pw_properties_set(properties, PW_KEY_TARGET_OBJECT, serial);
    }
    session->stream = pw_stream_new(session->core, "Amaterasu portal capture", properties);
    if (!session->stream) return FALSE;
    pw_stream_add_listener(session->stream, &session->stream_listener, &stream_events, session);
    uint8_t storage[1024];
    struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    const struct spa_pod *parameter = spa_pod_builder_add_object(&builder,
        SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat,
        SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video),
        SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
        SPA_FORMAT_VIDEO_format, SPA_POD_CHOICE_ENUM_Id(3, SPA_VIDEO_FORMAT_BGRA, SPA_VIDEO_FORMAT_BGRA, SPA_VIDEO_FORMAT_BGRx),
        /* Preserve the compositor's advertised source size. The video module
         * fits the complete image to the client's requested stream dimensions. */
        SPA_FORMAT_VIDEO_framerate, SPA_POD_CHOICE_RANGE_Fraction(&SPA_FRACTION(60, 1), &SPA_FRACTION(0, 1), &SPA_FRACTION(240, 1)));
    gboolean connected = pw_stream_connect(session->stream, PW_DIRECTION_INPUT,
        serial_value ? PW_ID_ANY : atomic_load(&session->node),
        PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS, &parameter, 1) >= 0;
    if (connected) atomic_store(&session->stage, AM_PW_STAGE_WAITING_FOR_FRAMES);
    return connected;
}

static void cleanup(struct am_pw_session *session) {
    if (session->stream) { spa_hook_remove(&session->stream_listener); pw_stream_destroy(session->stream); }
    if (session->core) { spa_hook_remove(&session->core_listener); pw_core_disconnect(session->core); }
    if (session->context) pw_context_destroy(session->context);
    if (session->loop) pw_main_loop_destroy(session->loop);
#ifdef AMATERASU_KWIN
    if (session->kwin) am_kwin_close(session->kwin);
#endif
    if (session->bus) {
        if (session->closed_subscription) g_dbus_connection_signal_unsubscribe(session->bus, session->closed_subscription);
        close_object(session, session->request_path, "org.freedesktop.portal.Request");
        close_object(session, session->session_path, "org.freedesktop.portal.Session");
        g_object_unref(session->bus);
    }
    g_clear_pointer(&session->session_path, g_free);
    g_clear_pointer(&session->request_path, g_free);
}

static GMutex initialization_lock;
static unsigned active_workers;
static void acquire_pipewire(void) {
    g_mutex_lock(&initialization_lock);
    if (active_workers++ == 0) pw_init(NULL, NULL);
    g_mutex_unlock(&initialization_lock);
}
static void release_pipewire(void) {
    g_mutex_lock(&initialization_lock);
    if (--active_workers == 0) pw_deinit();
    g_mutex_unlock(&initialization_lock);
}

static gpointer worker(gpointer data) {
    struct am_pw_session *session = data;
    session->main_context = g_main_context_new();
    g_main_context_push_thread_default(session->main_context);
    atomic_store(&session->stage, AM_PW_STAGE_CONNECTING_SESSION_BUS);
    gint64 deadline = g_get_monotonic_time() + (gint64)session->timeout_seconds * G_USEC_PER_SEC;
    acquire_pipewire();
    gboolean selected = FALSE;
    int remote = -1;
    if (session->named_output) {
#ifdef AMATERASU_KWIN
        session->kwin = am_kwin_open(session->named_output, &session->stopped, deadline);
        uint32_t node = 0;
        uint64_t serial = 0;
        struct am_kwin_geometry geometry = {0};
        selected = session->kwin && am_kwin_snapshot(session->kwin, &node, &serial, &geometry) == 0;
        if (selected) {
            g_mutex_lock(&session->geometry_lock); session->geometry = geometry; g_mutex_unlock(&session->geometry_lock);
            atomic_store(&session->node, node);
            atomic_store(&session->node_present, node != 0);
            atomic_store(&session->serial, serial);
            atomic_store(&session->serial_present, serial != 0);
        } else fail(session, 8);
#else
        fail(session, 8);
#endif
    } else {
        session->bus = connect_bus(session->cancel);
        if (!session->bus) fail(session, 1);
        else if (select_portal_source(session, deadline) && !atomic_load(&session->stopped)) {
            remote = portal_remote(session);
            selected = remote >= 0;
            if (!selected) fail(session, 4);
        }
    }
    if (selected && !atomic_load(&session->stopped)) {
        if (!connect_pipewire(session, remote)) fail(session, 4);
        else while (!atomic_load(&session->stopped)) {
            while (g_main_context_iteration(session->main_context, FALSE)) {}
#ifdef AMATERASU_KWIN
            if (session->kwin) {
                if (am_kwin_poll(session->kwin) < 0) { fail(session, 8); break; }
                struct am_kwin_geometry geometry = {0}; uint32_t node = 0; uint64_t serial = 0;
                if (am_kwin_snapshot(session->kwin, &node, &serial, &geometry) == 0) {
                    g_mutex_lock(&session->geometry_lock); session->geometry = geometry; g_mutex_unlock(&session->geometry_lock);
                }
            }
#endif
            uint32_t state = atomic_load(&session->state);
            if (state == AM_PW_FAILED || state == AM_PW_REVOKED) break;
            if (state != AM_PW_READY && g_get_monotonic_time() >= deadline) { fail(session, 7); break; }
            if (pw_loop_iterate(pw_main_loop_get_loop(session->loop), 5) < 0) { fail(session, 4); break; }
        }
    } else if (remote >= 0) close(remote);
    cleanup(session);
    release_pipewire();
    g_main_context_pop_thread_default(session->main_context);
    g_main_context_unref(session->main_context);
    return NULL;
}

static struct am_pw_session *start(uint32_t source_type, uint32_t timeout_seconds,
    const char *restore_token, uint32_t persist, am_pw_token_callback token_callback,
    am_pw_frame_callback callback, void *context, const char *named_output) {
    if ((source_type != 1 && source_type != 2) || !callback || timeout_seconds < 1 || timeout_seconds > 300) return NULL;
    struct am_pw_session *session = g_new0(struct am_pw_session, 1);
    g_mutex_init(&session->geometry_lock);
    atomic_init(&session->stopped, false);
    atomic_init(&session->state, AM_PW_SELECTING);
    atomic_init(&session->stage, AM_PW_STAGE_CONNECTING_SESSION_BUS);
    atomic_init(&session->node, 0);
    atomic_init(&session->width, 0);
    atomic_init(&session->height, 0);
    atomic_init(&session->format, 0);
    atomic_init(&session->error, 0);
    atomic_init(&session->pipewire_state, INT32_MAX);
    atomic_init(&session->pipewire_error, 0);
    atomic_init(&session->node_present, 0);
    atomic_init(&session->serial_present, 0);
    atomic_init(&session->serial, 0);
    session->cancel = g_cancellable_new();
    session->named_output = named_output ? g_strdup(named_output) : NULL;
    session->source_type = source_type;
    session->timeout_seconds = timeout_seconds;
    session->callback = callback;
    session->restore_token = restore_token ? g_strdup(restore_token) : NULL;
    session->persist = persist;
    session->token_callback = token_callback;
    session->callback_context = context;
    session->thread = g_thread_try_new("amaterasu-portal", worker, session, NULL);
    if (!session->thread) { g_object_unref(session->cancel); g_free(session->restore_token); g_free(session->named_output); g_mutex_clear(&session->geometry_lock); g_free(session); return NULL; }
    return session;
}

struct am_pw_session *am_pw_start(uint32_t source_type, uint32_t timeout_seconds,
    const char *restore_token, uint32_t persist, am_pw_token_callback token_callback,
    am_pw_frame_callback callback, void *context) {
    return start(source_type, timeout_seconds, restore_token, persist, token_callback, callback, context, NULL);
}
#ifdef AMATERASU_KWIN
struct am_pw_session *am_pw_start_output(const char *output, uint32_t timeout_seconds,
    am_pw_frame_callback callback, void *context) {
    if (!output || strlen(output) != 9 || strncmp(output, "Virtual-", 8) != 0 || output[8] < '1' || output[8] > '8') return NULL;
    return start(1, timeout_seconds, NULL, 0, NULL, callback, context, output);
}
#endif

void am_pw_snapshot(struct am_pw_session *session, struct am_pw_snapshot *output) {
    if (!session || !output) return;
    *output = (struct am_pw_snapshot){ .state = atomic_load(&session->state),
        .stage = atomic_load(&session->stage),
        .node = atomic_load(&session->node), .width = atomic_load(&session->width),
        .height = atomic_load(&session->height), .format = atomic_load(&session->format),
        .error = atomic_load(&session->error), .pipewire_state = atomic_load(&session->pipewire_state),
        .node_present = atomic_load(&session->node_present),
        .serial_present = atomic_load(&session->serial_present),
        .pipewire_error = atomic_load(&session->pipewire_error),
        .serial = atomic_load(&session->serial) };
    g_mutex_lock(&session->geometry_lock);
    output->left=session->geometry.left; output->top=session->geometry.top;
    output->logical_width=session->geometry.width; output->logical_height=session->geometry.height;
    output->desktop_left=session->geometry.desktop_left; output->desktop_top=session->geometry.desktop_top;
    output->desktop_width=session->geometry.desktop_width; output->desktop_height=session->geometry.desktop_height;
    g_mutex_unlock(&session->geometry_lock);
}

void am_pw_destroy(struct am_pw_session *session) {
    if (!session) return;
    atomic_store(&session->stopped, true);
    g_cancellable_cancel(session->cancel);
    g_thread_join(session->thread);
    g_object_unref(session->cancel);
    g_free(session->restore_token);
    g_free(session->named_output);
    g_mutex_clear(&session->geometry_lock);
    g_free(session);
}
