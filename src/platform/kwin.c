#define _POSIX_C_SOURCE 200809L
#include "kwin.h"
#include "zkde-screencast-unstable-v1.h"
#include "xdg-output-unstable-v1.h"
#include <wayland-client.h>
#include <glib.h>
#include <errno.h>
#include <limits.h>
#include <poll.h>
#include <stdlib.h>
#include <string.h>

#define MAX_OUTPUTS 64
struct output {
    uint32_t global;
    struct wl_output *object;
    struct zxdg_output_v1 *xdg;
    char name[128];
    int32_t x,y,width,height;
    bool removed;
};
struct am_kwin {
    struct wl_display *display;
    struct wl_registry *registry;
    struct zkde_screencast_unstable_v1 *manager;
    struct zkde_screencast_stream_unstable_v1 *stream;
    struct zxdg_output_manager_v1 *xdg_manager;
    struct output outputs[MAX_OUTPUTS];
    size_t count,selected;
    uint32_t node;
    uint64_t serial;
    bool failed;
};
static void output_geometry(void *data,struct wl_output *object,int32_t x,int32_t y,int32_t pw,int32_t ph,int32_t sub,const char *make,const char *model,int32_t transform) {
    (void)data;(void)object;(void)x;(void)y;(void)pw;(void)ph;(void)sub;(void)make;(void)model;(void)transform;
}
static void output_mode(void *data,struct wl_output *object,uint32_t flags,int32_t width,int32_t height,int32_t refresh) { (void)data;(void)object;(void)flags;(void)width;(void)height;(void)refresh; }
static void output_done(void *data,struct wl_output *object) { (void)data;(void)object; }
static void output_scale(void *data,struct wl_output *object,int32_t scale) { (void)data;(void)object;(void)scale; }
static void output_name(void *data,struct wl_output *object,const char *name) {
    (void)object; struct output *output=data;
    if (name && strlen(name)<sizeof(output->name)) memcpy(output->name,name,strlen(name)+1);
}
static void output_description(void *data,struct wl_output *object,const char *description) { (void)data;(void)object;(void)description; }
static const struct wl_output_listener output_listener={.geometry=output_geometry,.mode=output_mode,.done=output_done,.scale=output_scale,.name=output_name,.description=output_description};
static void logical_position(void *data,struct zxdg_output_v1 *object,int32_t x,int32_t y) { (void)object;struct output *output=data;output->x=x;output->y=y; }
static void logical_size(void *data,struct zxdg_output_v1 *object,int32_t width,int32_t height) { (void)object;struct output *output=data;output->width=width;output->height=height; }
static void logical_done(void *data,struct zxdg_output_v1 *object) { (void)data;(void)object; }
static void logical_name(void *data,struct zxdg_output_v1 *object,const char *name) { (void)object;output_name(data,NULL,name); }
static void logical_description(void *data,struct zxdg_output_v1 *object,const char *description) { (void)data;(void)object;(void)description; }
static const struct zxdg_output_v1_listener logical_listener={.logical_position=logical_position,.logical_size=logical_size,.done=logical_done,.name=logical_name,.description=logical_description};
static void release_output(struct output *output) {
    if (output->xdg) zxdg_output_v1_destroy(output->xdg);
    if (output->object) {
        if (wl_output_get_version(output->object) >= 3) wl_output_release(output->object);
        else wl_output_destroy(output->object);
    }
    memset(output, 0, sizeof(*output));
}
static void global(void *data,struct wl_registry *registry,uint32_t name,const char *interface,uint32_t version) {
    struct am_kwin *connection=data;
    if (strcmp(interface,zkde_screencast_unstable_v1_interface.name)==0 && !connection->manager) {
        connection->manager=wl_registry_bind(registry,name,&zkde_screencast_unstable_v1_interface,version<6?version:6);
    } else if (strcmp(interface,zxdg_output_manager_v1_interface.name)==0 && !connection->xdg_manager) {
        connection->xdg_manager=wl_registry_bind(registry,name,&zxdg_output_manager_v1_interface,version<3?version:3);
    } else if (strcmp(interface,wl_output_interface.name)==0) {
        size_t index=0;
        while (index<connection->count && !connection->outputs[index].removed) ++index;
        if (index>=MAX_OUTPUTS || version<2) { connection->failed=true;return; }
        if (index==connection->count) ++connection->count;
        struct output *output=&connection->outputs[index];
        if (output->removed) release_output(output);
        output->global=name;
        output->object=wl_registry_bind(registry,name,&wl_output_interface,version<4?version:4);
        wl_output_add_listener(output->object,&output_listener,output);
        if (connection->xdg_manager) {
            output->xdg=zxdg_output_manager_v1_get_xdg_output(connection->xdg_manager,output->object);
            zxdg_output_v1_add_listener(output->xdg,&logical_listener,output);
        }
    }
}
static void global_remove(void *data,struct wl_registry *registry,uint32_t name) {
    (void)registry;struct am_kwin *connection=data;
    for (size_t i=0;i<connection->count;i++) if (connection->outputs[i].global==name) {
        connection->outputs[i].removed=true;
        if (i==connection->selected) connection->failed=true;
    }
}
static const struct wl_registry_listener registry_listener={.global=global,.global_remove=global_remove};
int am_kwin_poll(struct am_kwin *connection) {
    if (!connection || connection->failed) return -1;
    while (wl_display_prepare_read(connection->display)!=0) {
        if (wl_display_dispatch_pending(connection->display)<0) return -1;
    }
    if (wl_display_flush(connection->display)<0 && errno!=EAGAIN) { wl_display_cancel_read(connection->display);return -1; }
    struct pollfd socket={.fd=wl_display_get_fd(connection->display),.events=POLLIN};
    int ready=poll(&socket,1,0);
    if (ready>0 && (socket.revents&POLLIN)) {
        if (wl_display_read_events(connection->display)<0) return -1;
    } else {
        wl_display_cancel_read(connection->display);
        if ((ready<0 && errno!=EINTR) || (ready>0 && (socket.revents&(POLLERR|POLLHUP|POLLNVAL)))) return -1;
    }
    return wl_display_dispatch_pending(connection->display)<0 || connection->failed ? -1:0;
}
static void synchronized(void *data,struct wl_callback *callback,uint32_t serial) { (void)callback;(void)serial;*(bool *)data=true; }
static const struct wl_callback_listener sync_listener={.done=synchronized};
static int synchronize(struct am_kwin *connection,const atomic_bool *stopped,int64_t deadline) {
    bool done=false;
    struct wl_callback *callback=wl_display_sync(connection->display);
    if (!callback) return -1;
    wl_callback_add_listener(callback,&sync_listener,&done);
    while (!done && !atomic_load(stopped) && g_get_monotonic_time()<deadline) {
        if (am_kwin_poll(connection)<0) break;
        if (!done) g_usleep(1000);
    }
    wl_callback_destroy(callback);
    return done?0:-1;
}
static void stream_closed(void *data,struct zkde_screencast_stream_unstable_v1 *stream) { (void)stream;((struct am_kwin *)data)->failed=true; }
static void stream_created(void *data,struct zkde_screencast_stream_unstable_v1 *stream,uint32_t node) { (void)stream;((struct am_kwin *)data)->node=node; }
static void stream_failed(void *data,struct zkde_screencast_stream_unstable_v1 *stream,const char *error) { (void)stream;(void)error;((struct am_kwin *)data)->failed=true; }
static void stream_serial(void *data,struct zkde_screencast_stream_unstable_v1 *stream,uint32_t high,uint32_t low) { (void)stream;((struct am_kwin *)data)->serial=((uint64_t)high<<32)|low; }
static const struct zkde_screencast_stream_unstable_v1_listener stream_listener={.closed=stream_closed,.created=stream_created,.failed=stream_failed,.serial=stream_serial};
struct am_kwin *am_kwin_open(const char *output,const atomic_bool *stopped,int64_t deadline) {
    if (!output || !stopped) return NULL;
    struct am_kwin *connection=calloc(1,sizeof(*connection));
    if (!connection) return NULL;
    connection->selected=SIZE_MAX;
    connection->display=wl_display_connect(NULL);
    if (!connection->display) goto failure;
    connection->registry=wl_display_get_registry(connection->display);
    wl_registry_add_listener(connection->registry,&registry_listener,connection);
    if (synchronize(connection,stopped,deadline)<0 || !connection->manager || !connection->xdg_manager) goto failure;
    for (size_t i=0;i<connection->count;i++) if (!connection->outputs[i].xdg) {
        connection->outputs[i].xdg=zxdg_output_manager_v1_get_xdg_output(connection->xdg_manager,connection->outputs[i].object);
        zxdg_output_v1_add_listener(connection->outputs[i].xdg,&logical_listener,&connection->outputs[i]);
    }
    if (synchronize(connection,stopped,deadline)<0) goto failure;
    for (size_t i=0;i<connection->count;i++) if (!connection->outputs[i].removed && strcmp(connection->outputs[i].name,output)==0) connection->selected=i;
    if (connection->selected==SIZE_MAX) goto failure;
    connection->stream=zkde_screencast_unstable_v1_stream_output(connection->manager,connection->outputs[connection->selected].object,ZKDE_SCREENCAST_UNSTABLE_V1_POINTER_EMBEDDED);
    zkde_screencast_stream_unstable_v1_add_listener(connection->stream,&stream_listener,connection);
    while (!connection->node && !connection->serial && !atomic_load(stopped) && g_get_monotonic_time()<deadline) {
        if (am_kwin_poll(connection)<0) goto failure;
        g_usleep(1000);
    }
    if (!connection->node && !connection->serial) goto failure;
    return connection;
failure:
    am_kwin_close(connection);return NULL;
}
int am_kwin_snapshot(struct am_kwin *connection,uint32_t *node,uint64_t *serial,struct am_kwin_geometry *geometry) {
    if (!connection || !node || !serial || !geometry || connection->failed || connection->selected>=connection->count) return -1;
    int64_t left=INT32_MAX,top=INT32_MAX,right=INT32_MIN,bottom=INT32_MIN;
    for (size_t i=0;i<connection->count;i++) {
        const struct output *output=&connection->outputs[i];
        if (output->removed) continue;
        if (output->width<=0 || output->height<=0 || output->width>32768 || output->height>32768) return -1;
        if (output->x<left) left=output->x;
        if (output->y<top) top=output->y;
        if ((int64_t)output->x+output->width>right) right=(int64_t)output->x+output->width;
        if ((int64_t)output->y+output->height>bottom) bottom=(int64_t)output->y+output->height;
    }
    if (right<=left || bottom<=top || right-left>65536 || bottom-top>65536) return -1;
    const struct output *selected=&connection->outputs[connection->selected];
    *geometry=(struct am_kwin_geometry){.left=selected->x,.top=selected->y,.width=(uint32_t)selected->width,.height=(uint32_t)selected->height,
        .desktop_left=(int32_t)left,.desktop_top=(int32_t)top,.desktop_width=(uint32_t)(right-left),.desktop_height=(uint32_t)(bottom-top)};
    *node=connection->node;*serial=connection->serial;
    return 0;
}
void am_kwin_close(struct am_kwin *connection) {
    if (!connection) return;
    if (connection->stream) zkde_screencast_stream_unstable_v1_close(connection->stream);
    for (size_t i=0;i<connection->count;i++) {
        struct output *output=&connection->outputs[i];
        release_output(output);
    }
    if (connection->manager) zkde_screencast_unstable_v1_destroy(connection->manager);
    if (connection->xdg_manager) zxdg_output_manager_v1_destroy(connection->xdg_manager);
    if (connection->registry) wl_registry_destroy(connection->registry);
    if (connection->display) { wl_display_flush(connection->display);wl_display_disconnect(connection->display); }
    free(connection);
}

__attribute__((used)) const char amaterasu_kwin_marker[] = "AMATERASU_KWIN_CAPTURE=1";
