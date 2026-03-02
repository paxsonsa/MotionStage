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

/* Legacy mode constants (deprecated — use data flow / recording constants) */
#define MOTIONSTAGE_SWIFT_MODE_IDLE 0
#define MOTIONSTAGE_SWIFT_MODE_LIVE 1
#define MOTIONSTAGE_SWIFT_MODE_RECORDING 2
#define MOTIONSTAGE_SWIFT_MODE_PLAYBACK 3

/* Data flow state (3.0) */
#define MOTIONSTAGE_SWIFT_DATA_FLOW_IDLE 0
#define MOTIONSTAGE_SWIFT_DATA_FLOW_LIVE 1

/* Recording state (3.0) */
#define MOTIONSTAGE_SWIFT_RECORDING_INACTIVE 0
#define MOTIONSTAGE_SWIFT_RECORDING_RECORDING 1
#define MOTIONSTAGE_SWIFT_RECORDING_PLAYBACK 2

#define MOTIONSTAGE_SWIFT_FIELD_POSITION      0x01
#define MOTIONSTAGE_SWIFT_FIELD_ROTATION      0x02
#define MOTIONSTAGE_SWIFT_FIELD_VELOCITY      0x04
#define MOTIONSTAGE_SWIFT_FIELD_FOCAL_LENGTH  0x08
#define MOTIONSTAGE_SWIFT_FIELD_FOCUS_DISTANCE 0x10
#define MOTIONSTAGE_SWIFT_FIELD_APERTURE      0x20

/* Connection state (4.2) */
#define MOTIONSTAGE_SWIFT_CONNECTION_DISCONNECTED  0
#define MOTIONSTAGE_SWIFT_CONNECTION_CONNECTED     1
#define MOTIONSTAGE_SWIFT_CONNECTION_RECONNECTING  2
#define MOTIONSTAGE_SWIFT_CONNECTION_FAILED        3

/* Connection event (4.2) */
#define MOTIONSTAGE_SWIFT_EVENT_CONNECTED          0
#define MOTIONSTAGE_SWIFT_EVENT_DISCONNECTED       1
#define MOTIONSTAGE_SWIFT_EVENT_RECONNECTING       2
#define MOTIONSTAGE_SWIFT_EVENT_RECONNECT_FAILED   3

/* Legacy camera-specific motion frame (deprecated — use send_batch instead). */
typedef struct {
    float position[3];
    float rotation[4];
    float velocity[3];
    float focal_length;
    float focus_distance;
    float aperture;
    uint32_t field_mask;
} MotionFrameFFI;

typedef struct {
    uint32_t handshake_timeout_ms;   /* 0 = use default (5000) */
    uint32_t mode_reply_timeout_ms;  /* 0 = use default (5000) */
    uint32_t reset_scene_timeout_ms; /* 0 = use default (5000) */
} MotionStageClientConfig;

/**
 * Typed attribute descriptor for motionstage_swift_client_new_v3.
 *
 * value_type ordinal:
 *   0  = Bool
 *   1  = Int32
 *   2  = Float32
 *   3  = Float64
 *   4  = Vec2f
 *   5  = Vec3f
 *   6  = Vec4f
 *   7  = Quatf
 *   8  = Mat4f
 *   9  = Trigger
 */
typedef struct {
    const char *path;           /* attribute path, e.g. "motion.position" */
    uint32_t value_type;        /* AttributeKind ordinal */
} MotionStageAttributeDescriptorC;

/**
 * General-purpose attribute update entry for motionstage_swift_client_send_batch.
 *
 * component_count encodes the value type:
 *   1  = Float32  (1 float)
 *   2  = Vec2f    (2 floats)
 *   3  = Vec3f    (3 floats)
 *   4  = Quatf    (4 floats: x y z w)
 *   16 = Mat4f    (16 floats, row-major)
 */
typedef struct {
    const char *attribute;      /* attribute path, e.g. "motion.position" */
    const float *data;          /* packed float values */
    uint32_t component_count;   /* number of floats pointed to by data */
} MotionAttributeUpdateC;

/**
 * Callback invoked when an async connect completes.
 * status: MOTIONSTAGE_SWIFT_STATUS_* code.
 * error:  human-readable error string (NULL on success). Do NOT free this pointer.
 * context: the opaque pointer passed to motionstage_swift_client_connect_async.
 * The callback is called from a Tokio worker thread.
 */
typedef void (*MotionStageConnectCallback)(int32_t status, const char *error, void *context);

/**
 * Connection event callback (4.2).
 * event: MOTIONSTAGE_SWIFT_EVENT_* constant.
 * attempt: reconnect attempt number (0 if not applicable).
 * message: human-readable string (NULL when not applicable). Do NOT free.
 * context: the opaque pointer passed to motionstage_swift_client_set_connection_event_callback.
 */
typedef void (*MotionStageConnectionEventCallback)(
    int32_t event, uint32_t attempt, const char *message, void *context);

/* Runtime (2.3) */

/**
 * Pre-initialize the shared Tokio runtime with a custom worker thread count.
 * Call before creating any client if you want a specific thread count.
 * thread_count=0 uses the Tokio default (number of CPU cores).
 * Has no effect if the runtime is already initialized.
 */
void motionstage_swift_runtime_init(uint32_t thread_count);

/* Client lifecycle */

void *motionstage_swift_client_new(
    const char *device_name,
    const char *output_attribute
);

/* Deprecated: prefer motionstage_swift_client_new_v2 */
void *motionstage_swift_client_new_multi(
    const char *device_name,
    const char *output_attributes_csv
);

void *motionstage_swift_client_new_multi_with_config(
    const char *device_name,
    const char *output_attributes_csv,
    const MotionStageClientConfig *config  /* NULL = use defaults */
);

/**
 * Array-based constructor (2.2). Preferred over motionstage_swift_client_new_multi.
 * attribute_names: array of attribute_count null-terminated C strings.
 */
void *motionstage_swift_client_new_v2(
    const char *device_name,
    uint32_t attribute_count,
    const char *const *attribute_names
);

/**
 * Typed descriptor constructor (3.0). Preferred over _new_v2.
 * attributes: array of attribute_count MotionStageAttributeDescriptorC entries.
 */
void *motionstage_swift_client_new_v3(
    const char *device_name,
    uint32_t attribute_count,
    const MotionStageAttributeDescriptorC *attributes
);

void motionstage_swift_client_free(void *client);

/* Connection */

int32_t motionstage_swift_client_connect(
    void *client,
    const char *server_addr,
    const char *pairing_token,
    const char *api_key
);

/**
 * Non-blocking connect. Spawns an async task; calls callback when done.
 * pairing_token and api_key may be NULL.
 * client must remain valid until callback is called.
 */
void motionstage_swift_client_connect_async(
    void *client,
    const char *server_addr,
    const char *pairing_token,  /* NULL = none */
    const char *api_key,        /* NULL = none */
    MotionStageConnectCallback callback,
    void *context               /* passed through to callback */
);

/**
 * Connect using a pinned certificate fingerprint (TOFU / 3.1).
 * fingerprint_hex: 64-char hex SHA-256 of the server's DER certificate.
 */
int32_t motionstage_swift_client_connect_pinned(
    void *client,
    const char *server_addr,
    const char *pairing_token,  /* NULL = none */
    const char *api_key,        /* NULL = none */
    const char *fingerprint_hex
);

int32_t motionstage_swift_client_disconnect(void *client);

/* General batch send (2.1) */

/**
 * Send multiple attribute updates in a single datagram.
 * updates: array of update_count MotionAttributeUpdateC entries.
 */
int32_t motionstage_swift_client_send_batch(
    void *client,
    const MotionAttributeUpdateC *updates,
    uint32_t update_count
);

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

/* Mode (3.0 — prefer set_data_flow / set_recording) */

/**
 * Set the data flow state. Returns composite mode via out-parameters.
 * state: MOTIONSTAGE_SWIFT_DATA_FLOW_* constant.
 * out_data_flow: receives the active data flow state after the change.
 * out_recording: receives the active recording state after the change.
 */
int32_t motionstage_swift_client_set_data_flow(
    void *client,
    int32_t state,
    int32_t *out_data_flow,
    int32_t *out_recording
);

/**
 * Set the recording state. Returns composite mode via out-parameters.
 * state: MOTIONSTAGE_SWIFT_RECORDING_* constant.
 * out_data_flow: receives the active data flow state after the change.
 * out_recording: receives the active recording state after the change.
 */
int32_t motionstage_swift_client_set_recording(
    void *client,
    int32_t state,
    int32_t *out_data_flow,
    int32_t *out_recording
);

/* Deprecated: prefer set_data_flow / set_recording */
int32_t motionstage_swift_client_set_mode(
    void *client,
    int32_t requested_mode,
    int32_t *active_mode_out
);

/* Reconnection (4.2) */

/**
 * Set the auto-reconnect policy.
 * max_attempts=0 disables auto-reconnect.
 * backoff_factor_x100: multiplier * 100 (e.g. 200 = 2.0x).
 */
int32_t motionstage_swift_client_set_reconnect_policy(
    void *client,
    uint32_t max_attempts,
    uint32_t initial_delay_ms,
    uint32_t max_delay_ms,
    uint32_t backoff_factor_x100
);

/**
 * Register a callback for connection state events.
 * Set callback to NULL to clear.
 */
int32_t motionstage_swift_client_set_connection_event_callback(
    void *client,
    MotionStageConnectionEventCallback callback,  /* NULL = clear */
    void *context
);

/**
 * Returns the current connection state (MOTIONSTAGE_SWIFT_CONNECTION_* constant).
 */
int32_t motionstage_swift_client_connection_state(void *client);

/* Accessors */

/* OWNERSHIP: caller must free with motionstage_swift_string_free() */
char *motionstage_swift_client_session_id(void *client);
/* OWNERSHIP: caller must free with motionstage_swift_string_free() */
char *motionstage_swift_client_device_id(void *client);
/* OWNERSHIP: caller must free with motionstage_swift_string_free() */
char *motionstage_swift_client_last_error(void *client);

void motionstage_swift_string_free(char *value);

#ifdef __cplusplus
}
#endif

#endif
