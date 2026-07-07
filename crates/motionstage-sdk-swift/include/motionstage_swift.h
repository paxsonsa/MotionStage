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

/* Video SDP type constants */
#define MOTIONSTAGE_SWIFT_SDP_TYPE_OFFER  0
#define MOTIONSTAGE_SWIFT_SDP_TYPE_ANSWER 1

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

/**
 * State-event stream callback (operator plane, protocol 2.1).
 *
 * message_json: NUL-terminated UTF-8 JSON owned by the SDK for the duration of
 * the call. Do NOT free it; copy the string if you need it afterwards.
 * Invoked from a background SDK thread. Two message shapes are delivered:
 *
 * 1. Replicated state event (ControlMessage::StateEventMsg):
 *    {"kind":"state_event",
 *     "seq":7,                          // strictly monotonic mutation order
 *     "origin_session":"<uuid>"|null,   // session that caused the mutation;
 *                                       // your own ops echo back with your
 *                                       // session id (no echo suppression)
 *     "timestamp_ns":123,
 *     "event":{"type":"<Variant>","data":{...}}}
 *
 *    "type" is the protocol StateEvent variant name — one of: ModeChanged,
 *    SceneLoaded, SceneActivated, MappingCreated, MappingUpdated,
 *    MappingRemoved, MappingLockChanged, MappingReleased, BaselineApplied,
 *    SessionJoined, SessionLeft, RecordingStarted, RecordingStopped,
 *    TakeRegistered, TakeSelected, TakeDeleted, PlaybackChanged.
 *    "data" carries the variant's fields with their wire names, e.g.
 *    {"type":"ModeChanged","data":{"mode":{"data_flow":"Live","recording":"Inactive"}}}
 *    {"type":"MappingCreated","data":{"mapping":{"mapping_id":"<uuid>", ...}}}
 *
 * 2. Unsolicited world snapshot (handshake / resync / lag recovery):
 *    {"kind":"scene_snapshot","snapshot":{...SceneSnapshotPayload...}}
 *    with fields scenes, mappings, mode, active_scene, sessions, takes,
 *    playback and seq (events with seq <= snapshot.seq are already folded in).
 */
typedef void (*MotionStageStateEventCallback)(const char *message_json, void *context);

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

/**
 * Non-blocking connect with certificate pinning (TOFU / 3.1).
 * fingerprint_hex: 64-char hex SHA-256 of the server's DER certificate.
 * client must remain valid until callback is called.
 */
void motionstage_swift_client_connect_async_pinned(
    void *client,
    const char *server_addr,
    const char *pairing_token,  /* NULL = none */
    const char *api_key,        /* NULL = none */
    const char *fingerprint_hex,
    MotionStageConnectCallback callback,
    void *context               /* passed through to callback */
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

/* Video signaling */

/**
 * Query server-reported video stream status.
 * out_available: 1 when server reports an active stream heartbeat, else 0.
 * out_descriptor_set: 1 when a master video descriptor is configured, else 0.
 * out_peer_count: active peer count with attached video tracks.
 * out_last_frame_age_ms: milliseconds since last pushed frame, or -1 if unknown.
 */
int32_t motionstage_swift_client_video_get_status(
    void *client,
    int32_t *out_available,
    int32_t *out_descriptor_set,
    uint32_t *out_peer_count,
    int64_t *out_last_frame_age_ms
);

/**
 * Request a server video offer for this client session.
 * out_sdp_type receives MOTIONSTAGE_SWIFT_SDP_TYPE_*.
 * out_sdp receives an owned C string; free with motionstage_swift_string_free().
 */
int32_t motionstage_swift_client_video_create_offer(
    void *client,
    const char *stream_id,
    const char *track_id,
    int32_t *out_sdp_type,
    char **out_sdp
);

/**
 * Send an SDP message to the server for this client session.
 * sdp_type must be MOTIONSTAGE_SWIFT_SDP_TYPE_*.
 */
int32_t motionstage_swift_client_video_send_sdp(
    void *client,
    int32_t sdp_type,
    const char *sdp
);

/**
 * Send an ICE candidate to the server for this client session.
 * sdp_mid may be NULL.
 * sdp_mline_index: -1 means "none".
 */
int32_t motionstage_swift_client_video_send_ice(
    void *client,
    const char *candidate,
    const char *sdp_mid,
    int32_t sdp_mline_index
);

/**
 * Poll the next pending server video signal as JSON.
 * Returns NULL when no signal is available.
 * On success, caller owns the returned string and must free with motionstage_swift_string_free().
 * JSON shape:
 *  - {"kind":"sdp","from_device":"...","to_device":"...","sdp_type":"offer|answer","sdp":"..."}
 *  - {"kind":"ice","from_device":"...","to_device":"...","candidate":"...","sdp_mid":"...|null","sdp_mline_index":0|null}
 */
char *motionstage_swift_client_video_next_signal_json(void *client);

/* Operator plane (protocol 2.1) */

/**
 * Register a callback for the server->client state-event stream
 * (StateEventMsg envelopes plus unsolicited SceneSnapshots) delivered as JSON
 * (see MotionStageStateEventCallback for the schema). The first registration
 * spawns a background pump thread that lives until motionstage_swift_client_free.
 *
 * Set callback to NULL to unsubscribe. NULL reliably quiesces delivery: the
 * call blocks until any in-flight dispatch batch finishes, so on return the
 * previous `context` pointer is guaranteed no longer in use and is safe to
 * free. While unsubscribed the SDK DROPS incoming state-stream messages instead
 * of queueing them (an unsubscribed client never accumulates an unbounded
 * backlog); re-subscribing recovers full state via the normal lag->SceneSnapshot
 * resync. The callback runs on a background thread and MUST NOT reentrantly call
 * this function or motionstage_swift_client_free.
 */
int32_t motionstage_swift_client_set_state_event_callback(
    void *client,
    MotionStageStateEventCallback callback,  /* NULL = unsubscribe */
    void *context
);

/**
 * Create a mapping (operator plane).
 * source_device: UUID string, or NULL = this session's own device.
 * target_scene:  UUID string, or NULL = the active scene.
 * component_mask: NULL = all components; otherwise component_mask_len indices.
 * On MOTIONSTAGE_SWIFT_STATUS_OK, *out_result_json receives an owned JSON
 * string — {"ok":{MappingSummary}} on success or
 * {"err":{"code":"<RejectCode>","reason":"..."}} on a typed server rejection
 * (RejectCode is e.g. "RoleDenied" or "ServerBusy"). Free the string with
 * motionstage_swift_string_free(). MappingSummary fields: mapping_id,
 * source_device, source_output, target_scene, target_object,
 * target_attribute, component_mask (array|null), lock (bool).
 * Non-OK statuses signal transport/argument failures (see last_error).
 */
int32_t motionstage_swift_client_create_mapping(
    void *client,
    const char *source_device,   /* NULL = own device */
    const char *source_output,
    const char *target_scene,    /* NULL = active scene */
    const char *target_object,
    const char *target_attribute,
    const uint32_t *component_mask,  /* NULL = all components */
    uint32_t component_mask_len,
    char **out_result_json
);

/**
 * Replace a mapping's full definition (operator plane). Argument semantics
 * and result JSON are identical to motionstage_swift_client_create_mapping.
 */
int32_t motionstage_swift_client_update_mapping(
    void *client,
    const char *mapping_id,
    const char *source_device,   /* NULL = own device */
    const char *source_output,
    const char *target_scene,    /* NULL = active scene */
    const char *target_object,
    const char *target_attribute,
    const uint32_t *component_mask,  /* NULL = all components */
    uint32_t component_mask_len,
    char **out_result_json
);

/**
 * Remove a mapping (operator plane). On MOTIONSTAGE_SWIFT_STATUS_OK,
 * *out_result_json receives {"ok":null} or {"err":{...}}; free with
 * motionstage_swift_string_free().
 */
int32_t motionstage_swift_client_remove_mapping(
    void *client,
    const char *mapping_id,
    char **out_result_json
);

/**
 * Lock or unlock a mapping (operator plane). lock: 0 = unlock, else lock.
 * Result JSON as for motionstage_swift_client_remove_mapping.
 */
int32_t motionstage_swift_client_set_mapping_lock(
    void *client,
    const char *mapping_id,
    int32_t lock,
    char **out_result_json
);

/**
 * Start recording a take (Operator role required; take id and path are
 * server-assigned). On MOTIONSTAGE_SWIFT_STATUS_OK, *out_result_json receives
 * {"ok":"<take-uuid>"} or {"err":{...}}; free with
 * motionstage_swift_string_free().
 */
int32_t motionstage_swift_client_start_take(
    void *client,
    char **out_result_json
);

/**
 * Stop the active recording and register the take (Operator role required).
 * On MOTIONSTAGE_SWIFT_STATUS_OK, *out_result_json receives {"ok":{TakeInfo}}
 * or {"err":{...}}; free with motionstage_swift_string_free(). TakeInfo
 * fields: take_id, scene_id, name, path, created_ns, frame_count, selected,
 * deleted.
 */
int32_t motionstage_swift_client_stop_take(
    void *client,
    char **out_result_json
);

/**
 * Request an on-demand full world snapshot. On MOTIONSTAGE_SWIFT_STATUS_OK,
 * *out_snapshot_json receives the SceneSnapshotPayload as JSON (no {"ok":...}
 * envelope — the request has no typed wire error); free with
 * motionstage_swift_string_free().
 */
int32_t motionstage_swift_client_get_scene_snapshot(
    void *client,
    char **out_snapshot_json
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
