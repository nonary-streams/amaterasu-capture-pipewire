#ifndef AMATERASU_KWIN_CAPTURE_H
#define AMATERASU_KWIN_CAPTURE_H
#include <stdint.h>
#include <stdatomic.h>
#include <stdbool.h>
struct am_kwin;
struct am_kwin_geometry { int32_t left,top; uint32_t width,height; int32_t desktop_left,desktop_top; uint32_t desktop_width,desktop_height; };
/* Connection, Wayland proxies and event callbacks remain on one native worker.
 * The borrowed cancellation flag outlives open/poll and all callbacks. */
struct am_kwin *am_kwin_open(const char *output,const atomic_bool *stopped,int64_t deadline_us);
int am_kwin_poll(struct am_kwin *connection);
int am_kwin_snapshot(struct am_kwin *connection,uint32_t *node,uint64_t *serial,struct am_kwin_geometry *geometry);
void am_kwin_close(struct am_kwin *connection);
#endif
