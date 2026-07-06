// Wire types — must stay in sync with the `ServerToUi` / command schema in
// crates/motionstage-server/src/companion_ui.rs.

// AttributeValue is serde externally-tagged: {"Vec3f":[1,2,3]}, {"Float32":1.5}, ...
export type AttributeValue = Record<string, unknown>;
export type AttributeFilter = string | Record<string, unknown>;

export interface UiMode {
  data_flow: "Idle" | "Live";
  recording: "Inactive" | "Recording" | "Playback";
  label: "idle" | "live" | "recording" | "playback";
}

export interface UiAttribute {
  name: string;
  value_type: string;
  default_value: AttributeValue;
  current_value: AttributeValue;
  live_enabled: boolean;
  record_enabled: boolean;
  filter_chain: AttributeFilter[];
}

export interface UiObject {
  id: string;
  name: string;
  attributes: UiAttribute[];
}

export interface UiScene {
  id: string;
  name: string;
  objects: UiObject[];
}

export interface UiMapping {
  id: string;
  source_device: string;
  source_output: string;
  target_scene: string;
  target_object: string;
  target_attribute: string;
  component_mask: number[] | null;
  lock: boolean;
  state: "Active" | "Released";
}

export interface UiSceneState {
  active_scene: string | null;
  scenes: UiScene[];
  mappings: UiMapping[];
}

export interface UiSession {
  device_id: string;
  device_name: string;
  session_id: string | null;
  roles: string[];
  features: string[];
  advertised_attributes: { path: string; value_type: string }[];
  state: string;
}

export interface UiMetrics {
  accepted_sessions: number;
  rejected_sessions: number;
  motion_datagrams: number;
  motion_updates: number;
  signaling_messages: number;
  scheduler_ticks: number;
  publish_ticks: number;
}

export interface VideoStatus {
  available: boolean;
  descriptor_set: boolean;
  peer_count: number;
  last_frame_age_ms: number | null;
}

export interface UiTake {
  take_id: string;
  scene_id: string;
  name: string;
  frame_count: number;
  created_ns: number;
  selected: boolean;
}

export interface UiPlayback {
  take_id: string;
  state: "stopped" | "playing" | "paused";
  position_ns: number;
  duration_ns: number;
  looping: boolean;
}

export interface AppData {
  scene: UiSceneState;
  mode: UiMode;
  sessions: UiSession[];
  metrics: UiMetrics;
  video: VideoStatus;
  takes: UiTake[];
  playback: UiPlayback | null;
  selected?: string[]; // object names selected in the host DCC (Blender), for highlight
}

export type ServerToUi =
  | ({ type: "snapshot" } & AppData)
  | { type: "mode_changed"; mode: UiMode }
  | ({ type: "snapshot_changed" } & UiSceneState)
  | { type: "attribute_values"; changes: UiAttributeValue[] }
  | { type: "session_upserted"; session: UiSession }
  | { type: "session_removed"; device_id: string }
  | { type: "video_status_changed"; video: VideoStatus }
  | { type: "metrics"; metrics: UiMetrics }
  | { type: "takes_changed"; takes: UiTake[]; playback: UiPlayback | null }
  | { type: "selection_changed"; selected: string[] }
  | { type: "command_error"; message: string };

export interface UiAttributeValue {
  scene_id: string;
  object_id: string;
  attribute: string;
  value: AttributeValue;
}
