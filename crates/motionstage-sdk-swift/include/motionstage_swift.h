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
#define MOTIONSTAGE_SWIFT_MODE_PLAYBACK 3

#define MOTIONSTAGE_SWIFT_FIELD_POSITION      0x01
#define MOTIONSTAGE_SWIFT_FIELD_ROTATION      0x02
#define MOTIONSTAGE_SWIFT_FIELD_VELOCITY      0x04
#define MOTIONSTAGE_SWIFT_FIELD_FOCAL_LENGTH  0x08
#define MOTIONSTAGE_SWIFT_FIELD_FOCUS_DISTANCE 0x10
#define MOTIONSTAGE_SWIFT_FIELD_APERTURE      0x20

typedef struct {
    float position[3];
    float rotation[4];
    float velocity[3];
    float focal_length;
    float focus_distance;
    float aperture;
    uint32_t field_mask;
} MotionFrameFFI;

/* Client lifecycle */

void *motionstage_swift_client_new(
    const char *device_name,
    const char *output_attribute
);

void *motionstage_swift_client_new_multi(
    const char *device_name,
    const char *output_attributes_csv
);

void motionstage_swift_client_free(void *client);

/* Connection */

int32_t motionstage_swift_client_connect(
    void *client,
    const char *server_addr,
    const char *pairing_token,
    const char *api_key
);

int32_t motionstage_swift_client_disconnect(void *client);

/* Motion data (legacy single-attribute) */

int32_t motionstage_swift_client_send_vec3f(
    void *client,
    float x,
    float y,
    float z
);

/* Motion data (multi-attribute) */

int32_t motionstage_swift_client_send_motion_frame(
    void *client,
    const MotionFrameFFI *frame
);

int32_t motionstage_swift_client_send_named_vec3f(
    void *client,
    const char *attribute,
    float x,
    float y,
    float z
);

int32_t motionstage_swift_client_send_named_quatf(
    void *client,
    const char *attribute,
    float x,
    float y,
    float z,
    float w
);

int32_t motionstage_swift_client_send_named_float32(
    void *client,
    const char *attribute,
    float value
);

/* Scene control */

int32_t motionstage_swift_client_reset_scene(void *client);

/* Mode */

int32_t motionstage_swift_client_set_mode(
    void *client,
    int32_t requested_mode,
    int32_t *active_mode_out
);

/* Accessors */

char *motionstage_swift_client_session_id(void *client);
char *motionstage_swift_client_device_id(void *client);
char *motionstage_swift_client_last_error(void *client);

void motionstage_swift_string_free(char *value);

#ifdef __cplusplus
}
#endif

#endif
