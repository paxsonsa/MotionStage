"""
Type stubs for the motionstage_sdk_rust native extension (PyO3 bridge).

These document the raw tuple-based return types consumed by the Python wrapper
in motionstage_sdk/server.py. Type-checkers can use these stubs to validate
the bridge layer even without the compiled extension present.
"""

from typing import Any, Optional

class MotionStageServer:
    def __init__(
        self,
        name: str | None = None,
        discoverable: bool = True,
        bind_addr: str | None = None,
    ) -> None: ...

    def start(self) -> str: ...
    def stop(self) -> None: ...

    def host_session_id(self) -> str:
        """Session id of the in-process host session this bridge acts as.
        Host-API mutations carry this id as their event origin_session."""
        ...

    def host_device_id(self) -> str: ...

    def current_event_seq(self) -> int:
        """Sequence number of the most recently emitted state event."""
        ...

    def initial_scene_snapshot(self) -> dict[str, Any]:
        """Full world snapshot: {seq, mode, data_flow, recording, active_scene,
        scenes: [{scene_id, name, objects: [{object_id, name, attributes:
        [{name, default_value, current_value, live_enabled, record_enabled}]}]}],
        mappings: [{mapping_id, source_device, source_output, target_scene,
        target_object, target_attribute, component_mask, lock}],
        sessions: [{session_id, device_id, device_name, roles, is_host}]
        (registered sessions only, the in-process host included),
        takes: [{take_id, scene_id, name, path, created_ns, frame_count,
        selected, deleted}],
        playback: {state, take_id, playhead_ns, looping} | None}.
        Events with seq <= the snapshot's seq are already folded in."""
        ...

    def drain_state_events(self, timeout_ms: int = 0) -> list[dict[str, Any]]:
        """Block up to timeout_ms for the next state event, then batch everything
        currently queued (timeout_ms == 0 is a non-blocking drain). Each dict has
        seq, origin_session (str | None), timestamp_ns, a snake_case "type"
        discriminator (mode_changed, scene_loaded, scene_activated,
        mapping_created/updated/removed/lock_changed/released, baseline_applied,
        session_joined, session_left, recording_started/stopped, take_registered,
        take_selected, take_deleted, playback_changed), and the event payload
        flattened alongside. A lagged receiver yields
        {"type": "event_stream_lagged", "skipped": n}; refetch
        initial_scene_snapshot when one appears."""
        ...

    def upsert_scene(self, spec: dict[str, Any]) -> str: ...
    def set_active_scene(self, scene_id: str) -> None: ...

    def set_live_mode(self) -> None: ...
    def set_stopped_mode(self) -> None: ...
    def set_mode(self, mode: str) -> str: ...
    def mode(self) -> str: ...

    def set_mode_control_allowlist(self, device_ids: list[str]) -> None: ...
    def mode_control_allowlist(self) -> list[str]: ...

    def metrics(self) -> tuple[int, int, int, int, int, int, int]:
        """Returns (accepted_sessions, rejected_sessions, motion_datagrams,
        motion_updates, signaling_messages, scheduler_ticks, publish_ticks)."""
        ...

    def start_recording(self, path: str) -> str: ...
    def stop_recording(self) -> None: ...

    def list_takes(
        self, scene_id: str | None = None
    ) -> list[tuple[str, str, str, str, int, int, bool, bool]]:
        """Returns list of (take_id, scene_id, name, path, created_ns, frame_count, selected, deleted)."""
        ...

    def select_take(self, take_id: str) -> str: ...
    def delete_take(self, take_id: str) -> None: ...

    def playback_play(self, take_id: str, looping: bool = False) -> tuple[str, int, bool]:
        """Returns (state, playhead_ns, looping)."""
        ...

    def playback_pause(self, take_id: str) -> tuple[str, int, bool]:
        """Returns (state, playhead_ns, looping)."""
        ...

    def playback_seek(
        self, take_id: str, seek_ns: int, looping: bool = False
    ) -> tuple[str, int, bool]:
        """Returns (state, playhead_ns, looping)."""
        ...

    def playback_stop(self, take_id: str) -> tuple[str, int, bool]:
        """Returns (state, playhead_ns, looping)."""
        ...

    def open_take_bake_cursor(
        self, take_id: str, sampling_mode: str = "captured"
    ) -> tuple[str, int]:
        """Returns (cursor_id, total_frames)."""
        ...

    def read_take_bake_frame(
        self, cursor_id: str
    ) -> Optional[tuple[int, int, list[tuple[str, str, object]]]]:
        """Returns None at end-of-stream, else (frame_index, timestamp_ns, attributes).
        Each attribute is (object_id, attribute_name, value)."""
        ...

    def seek_take_bake_frame(
        self, cursor_id: str, frame_index: int
    ) -> Optional[tuple[int, int, list[tuple[str, str, object]]]]:
        """Returns None if out of range, else (resolved_frame_index, timestamp_ns, attributes).
        Each attribute is (object_id, attribute_name, value)."""
        ...

    def close_take_bake_cursor(self, cursor_id: str) -> None: ...

    def sessions(
        self,
    ) -> list[tuple[str, str, str | None, list[str], list[str], list[str], str, bool]]:
        """Raw diagnostics listing of every session record (all lifecycle
        states, the in-process host session included). Returns list of
        (device_id, device_name, session_id, roles, features,
        advertised_attributes, state, is_host)."""
        ...

    def create_mapping(self, request: dict[str, Any]) -> str: ...
    def remove_mapping(self, mapping_id: str) -> None: ...

    def reset_scene_to_baseline(self, scene_id: str | None = None) -> int: ...
    def commit_scene_baseline(self, scene_id: str | None = None) -> int: ...
    def commit_object_baseline(self, object_id: str, scene_id: str | None = None) -> int: ...

    def runtime_attribute_values(self) -> list[tuple[str, str, str, object]]:
        """Returns list of (object_id, object_name, attribute_name, value)."""
        ...

    def set_master_video_descriptor(self, width: int, height: int, fps: int) -> None: ...
    def push_video_frame(self, frame_data: bytes, timestamp_ns: int) -> None: ...
    def push_video_frame_bgra(self, frame_data: bytes, timestamp_ns: int) -> None: ...
    def video_peer_count(self) -> int: ...
    def start_companion_ui(self) -> int: ...
    def companion_ui_token(self) -> str | None: ...
    def stop_companion_ui(self) -> None: ...
    def drain_host_requests(self) -> list[dict[str, object]]: ...
    def set_host_selection(self, names: list[str]) -> None: ...
