#ifndef MOTIONSTAGE_SWIFT_H
#define MOTIONSTAGE_SWIFT_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#define MOTIONSTAGE_SWIFT_STATUS_OK 0
#define MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT 1
#define MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED 2
#define MOTIONSTAGE_SWIFT_STATUS_ALREADY_CONNECTED 3
#define MOTIONSTAGE_SWIFT_STATUS_PROTOCOL 4
#define MOTIONSTAGE_SWIFT_STATUS_TRANSPORT 5
#define MOTIONSTAGE_SWIFT_STATUS_INTERNAL 6

#define MOTIONSTAGE_SWIFT_MODE_IDLE 0
#define MOTIONSTAGE_SWIFT_MODE_LIVE 1
#define MOTIONSTAGE_SWIFT_MODE_RECORDING 2

void *motionstage_swift_client_new(
    const char *device_name,
    const char *output_attribute
);

void motionstage_swift_client_free(void *client);

int32_t motionstage_swift_client_connect(
    void *client,
    const char *server_addr,
    const char *pairing_token,
    const char *api_key
);

int32_t motionstage_swift_client_disconnect(void *client);

int32_t motionstage_swift_client_send_vec3f(
    void *client,
    float x,
    float y,
    float z
);

int32_t motionstage_swift_client_set_mode(
    void *client,
    int32_t requested_mode,
    int32_t *active_mode_out
);

char *motionstage_swift_client_session_id(void *client);
char *motionstage_swift_client_device_id(void *client);
char *motionstage_swift_client_last_error(void *client);

void motionstage_swift_string_free(char *value);

#ifdef __cplusplus
}
#endif

#endif
