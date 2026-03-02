from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
import logging
import threading
import time
from typing import Any, Optional
from uuid import UUID

from .delegates import SceneUpdateDelegate
from .video import VideoStreamDescriptor

try:  # pragma: no cover - exercised only when extension module is present
    from motionstage_sdk_rust import MotionStageServer as _NativeMotionStageServer
except Exception:  # pragma: no cover
    _NativeMotionStageServer = None

LOGGER = logging.getLogger("motionstage_sdk.server")


class SecurityMode(str, Enum):
    TRUSTED_LAN = "trusted_lan"
    PAIRING_REQUIRED = "pairing_required"
    API_KEY = "api_key"
    API_KEY_PLUS_PAIRING = "api_key_plus_pairing"


@dataclass
class MotionStageSession:
    device_id: UUID
    device_name: str
    session_id: UUID | None
    roles: tuple[str, ...]
    features: tuple[str, ...]
    advertised_attributes: tuple[str, ...]
    state: str


@dataclass(frozen=True)
class ServerMetrics:
    accepted_sessions: int
    rejected_sessions: int
    motion_datagrams: int
    motion_updates: int
    signaling_messages: int
    scheduler_ticks: int
    publish_ticks: int


@dataclass(frozen=True)
class TakeInfo:
    take_id: str
    scene_id: str
    name: str
    path: str
    created_ns: int
    frame_count: int
    selected: bool
    deleted: bool


@dataclass(frozen=True)
class PlaybackState:
    state: str
    playhead_ns: int
    looping: bool


@dataclass(frozen=True)
class BakeCursorInfo:
    cursor_id: str
    total_frames: int


@dataclass(frozen=True)
class BakeAttributeValue:
    object_id: str
    object_name: str
    attribute_name: str
    value: object


@dataclass(frozen=True)
class BakeFrame:
    frame_index: int
    timestamp_ns: int
    attributes: list[BakeAttributeValue]


@dataclass
class MappingManager:
    _owner: "MotionStageServer"

    def create_mapping(self, request: dict[str, Any]) -> UUID:
        return self._owner.create_mapping(request)

    def remove_mapping(self, mapping_id: UUID) -> None:
        self._owner.remove_mapping(mapping_id)


@dataclass
class RecordingController:
    _owner: "MotionStageServer"
    is_recording: bool = False
    active_path: Optional[str] = None

    def start_recording(self, path: str) -> None:
        self._owner.start_recording(path)
        self.is_recording = True
        self.active_path = path

    def stop_recording(self) -> None:
        self._owner.stop_recording()
        self.is_recording = False
        self.active_path = None


@dataclass
class TakeController:
    _owner: "MotionStageServer"

    def list_takes(self, scene_id: UUID | None = None) -> list[TakeInfo]:
        return self._owner.list_takes(scene_id)

    def select_take(self, take_id: UUID) -> UUID:
        return self._owner.select_take(take_id)

    def delete_take(self, take_id: UUID) -> None:
        self._owner.delete_take(take_id)

    def playback_play(self, take_id: UUID, looping: bool = False) -> PlaybackState:
        return self._owner.playback_play(take_id, looping=looping)

    def playback_pause(self, take_id: UUID) -> PlaybackState:
        return self._owner.playback_pause(take_id)

    def playback_seek(self, take_id: UUID, seek_ns: int, looping: bool = False) -> PlaybackState:
        return self._owner.playback_seek(take_id, seek_ns, looping=looping)

    def playback_stop(self, take_id: UUID) -> PlaybackState:
        return self._owner.playback_stop(take_id)

    def open_bake_cursor(self, take_id: UUID, sampling_mode: str = "captured") -> BakeCursorInfo:
        return self._owner.open_take_bake_cursor(take_id, sampling_mode=sampling_mode)

    def read_bake_frame(self, cursor_id: UUID) -> Optional[BakeFrame]:
        return self._owner.read_take_bake_frame(cursor_id)

    def seek_bake_frame(self, cursor_id: UUID, frame_index: int) -> Optional[BakeFrame]:
        return self._owner.seek_take_bake_frame(cursor_id, frame_index)

    def close_bake_cursor(self, cursor_id: UUID) -> None:
        self._owner.close_take_bake_cursor(cursor_id)


class MotionStageServer:
    def __init__(self, name: str = "motionstage", security: SecurityMode = SecurityMode.TRUSTED_LAN):
        if _NativeMotionStageServer is None:
            raise RuntimeError(
                "motionstage_sdk_rust native extension is required; install/rebuild the SDK in "
                "the current Python environment (for Blender, install into Blender's Python)."
            )
        self.name = name
        self.security = security
        self.mapping_manager = MappingManager(self)
        self.recording = RecordingController(self)
        self.takes = TakeController(self)
        self._delegate: Optional[SceneUpdateDelegate] = None
        self._native = _NativeMotionStageServer(name=name)
        self._running = False
        # Fixed high-performance defaults; avoid runtime tuning knobs.
        self._session_poll_interval_s = 1.0
        self._mode_poll_interval_s = 0.05
        self._attribute_poll_interval_live_s = 1.0 / 120.0
        self._attribute_poll_interval_idle_s = 0.25
        self._event_loop_tick_s = 0.005
        # Back-compat alias; kept for external diagnostics expecting this field.
        self._event_poll_interval_s = self._session_poll_interval_s
        self._event_thread: Optional[threading.Thread] = None
        self._event_stop = threading.Event()
        self._known_clients: dict[str, tuple[str, tuple[str, ...], tuple[str, ...], tuple[str, ...], str]] = {}
        self._known_mode: Optional[str] = None
        self._known_attribute_values: dict[tuple[str, str], str] = {}

    def bind_delegate(self, delegate: SceneUpdateDelegate) -> None:
        self._delegate = delegate
        if self._running:
            self._start_event_pump()

    def start(self) -> str:
        self._running = True
        endpoint = str(self._native.start())
        self._start_event_pump()
        return endpoint

    def stop(self) -> None:
        self._running = False
        self._stop_event_pump()
        self._native.stop()

    def upsert_scene(self, scene: dict[str, Any]) -> UUID:
        scene_id = self._native.upsert_scene(scene)
        return UUID(str(scene_id))

    def set_active_scene(self, scene_id: UUID) -> None:
        self._native.set_active_scene(str(scene_id))

    def set_live_mode(self) -> None:
        self._native.set_mode("live")

    def set_stopped_mode(self) -> None:
        self._native.set_mode("idle")

    def set_mode(self, mode: str) -> str:
        return str(self._native.set_mode(mode))

    def mode(self) -> str:
        return str(self._native.mode())

    def set_mode_control_allowlist(self, device_ids: list[UUID]) -> None:
        self._native.set_mode_control_allowlist([str(device_id) for device_id in device_ids])

    def mode_control_allowlist(self) -> list[UUID]:
        return [UUID(str(device_id)) for device_id in self._native.mode_control_allowlist()]

    def metrics(self) -> ServerMetrics:
        raw = self._native.metrics()
        return ServerMetrics(
            accepted_sessions=raw[0],
            rejected_sessions=raw[1],
            motion_datagrams=raw[2],
            motion_updates=raw[3],
            signaling_messages=raw[4],
            scheduler_ticks=raw[5],
            publish_ticks=raw[6],
        )

    def start_recording(self, path: str) -> UUID:
        recording_id = self._native.start_recording(path)
        return UUID(str(recording_id))

    def stop_recording(self) -> None:
        self._native.stop_recording()

    def list_takes(self, scene_id: UUID | None = None) -> list[TakeInfo]:
        raw_scene_id = str(scene_id) if scene_id is not None else None
        rows = self._native.list_takes(raw_scene_id)
        takes: list[TakeInfo] = []
        for row in rows:
            (
                take_id,
                resolved_scene_id,
                name,
                path,
                created_ns,
                frame_count,
                selected,
                deleted,
            ) = row
            takes.append(
                TakeInfo(
                    take_id=str(take_id),
                    scene_id=str(resolved_scene_id),
                    name=str(name),
                    path=str(path),
                    created_ns=int(created_ns),
                    frame_count=int(frame_count),
                    selected=bool(selected),
                    deleted=bool(deleted),
                )
            )
        return takes

    def select_take(self, take_id: UUID) -> UUID:
        selected = self._native.select_take(str(take_id))
        return UUID(str(selected))

    def playback_play(self, take_id: UUID, looping: bool = False) -> PlaybackState:
        state, playhead_ns, loop_state = self._native.playback_play(str(take_id), bool(looping))
        return PlaybackState(state=str(state), playhead_ns=int(playhead_ns), looping=bool(loop_state))

    def playback_pause(self, take_id: UUID) -> PlaybackState:
        state, playhead_ns, loop_state = self._native.playback_pause(str(take_id))
        return PlaybackState(state=str(state), playhead_ns=int(playhead_ns), looping=bool(loop_state))

    def playback_seek(self, take_id: UUID, seek_ns: int, looping: bool = False) -> PlaybackState:
        state, playhead_ns, loop_state = self._native.playback_seek(
            str(take_id), int(seek_ns), bool(looping)
        )
        return PlaybackState(state=str(state), playhead_ns=int(playhead_ns), looping=bool(loop_state))

    def playback_stop(self, take_id: UUID) -> PlaybackState:
        state, playhead_ns, loop_state = self._native.playback_stop(str(take_id))
        return PlaybackState(state=str(state), playhead_ns=int(playhead_ns), looping=bool(loop_state))

    def delete_take(self, take_id: UUID) -> None:
        self._native.delete_take(str(take_id))

    def open_take_bake_cursor(self, take_id: UUID, sampling_mode: str = "captured") -> BakeCursorInfo:
        cursor_id, total_frames = self._native.open_take_bake_cursor(str(take_id), str(sampling_mode))
        return BakeCursorInfo(cursor_id=str(cursor_id), total_frames=int(total_frames))

    def read_take_bake_frame(self, cursor_id: UUID) -> Optional[BakeFrame]:
        row = self._native.read_take_bake_frame(str(cursor_id))
        if row is None:
            return None
        frame_index, timestamp_ns, attrs = row
        return BakeFrame(
            frame_index=int(frame_index),
            timestamp_ns=int(timestamp_ns),
            attributes=[
                BakeAttributeValue(
                    object_id=str(object_id),
                    object_name="",
                    attribute_name=str(attribute),
                    value=value,
                )
                for (object_id, attribute, value) in attrs
            ],
        )

    def seek_take_bake_frame(self, cursor_id: UUID, frame_index: int) -> Optional[BakeFrame]:
        row = self._native.seek_take_bake_frame(str(cursor_id), int(frame_index))
        if row is None:
            return None
        resolved_index, timestamp_ns, attrs = row
        return BakeFrame(
            frame_index=int(resolved_index),
            timestamp_ns=int(timestamp_ns),
            attributes=[
                BakeAttributeValue(
                    object_id=str(object_id),
                    object_name="",
                    attribute_name=str(attribute),
                    value=value,
                )
                for (object_id, attribute, value) in attrs
            ],
        )

    def close_take_bake_cursor(self, cursor_id: UUID) -> None:
        self._native.close_take_bake_cursor(str(cursor_id))

    def sessions(self) -> list[dict[str, Any]]:
        rows = self._native.sessions()
        sessions: list[dict[str, Any]] = []
        for row in rows:
            (
                device_id,
                device_name,
                session_id,
                roles,
                features,
                advertised_attributes,
                state,
            ) = row
            sessions.append(
                {
                    "device_id": str(device_id),
                    "device_name": str(device_name),
                    "session_id": str(session_id) if session_id else None,
                    "roles": [str(value) for value in roles],
                    "features": [str(value) for value in features],
                    "advertised_attributes": [str(value) for value in advertised_attributes],
                    "state": str(state),
                }
            )
        return sessions

    def create_mapping(self, request: dict[str, Any]) -> UUID:
        normalized = dict(request)
        normalized["source_device"] = str(normalized["source_device"])
        normalized["target_object_id"] = str(normalized["target_object_id"])
        if normalized.get("target_scene") is not None:
            normalized["target_scene"] = str(normalized["target_scene"])
        mapping_id = self._native.create_mapping(normalized)
        return UUID(str(mapping_id))

    def remove_mapping(self, mapping_id: UUID) -> None:
        self._native.remove_mapping(str(mapping_id))

    def reset_scene_to_baseline(self, scene_id: UUID | None = None) -> int:
        raw_scene_id = str(scene_id) if scene_id is not None else None
        return int(self._native.reset_scene_to_baseline(raw_scene_id))

    def commit_scene_baseline(self, scene_id: UUID | None = None) -> int:
        raw_scene_id = str(scene_id) if scene_id is not None else None
        return int(self._native.commit_scene_baseline(raw_scene_id))

    def commit_object_baseline(self, object_id: UUID, scene_id: UUID | None = None) -> int:
        raw_scene_id = str(scene_id) if scene_id is not None else None
        return int(self._native.commit_object_baseline(str(object_id), raw_scene_id))

    def runtime_attribute_values(self) -> list[BakeAttributeValue]:
        rows = self._native.runtime_attribute_values()
        values: list[BakeAttributeValue] = []
        for row in rows:
            object_id = ""
            if isinstance(row, (list, tuple)) and len(row) == 4:
                object_id, object_name, attribute_name, value = row
            elif isinstance(row, (list, tuple)) and len(row) == 3:
                object_name, attribute_name, value = row
            else:
                continue
            values.append(
                BakeAttributeValue(
                    object_id=str(object_id).strip() if object_id is not None else "",
                    object_name=str(object_name),
                    attribute_name=str(attribute_name),
                    value=value,
                )
            )
        return values

    # --- Video ---

    def set_master_video_descriptor(self, descriptor: VideoStreamDescriptor) -> None:
        self._native.set_master_video_descriptor(
            descriptor.width, descriptor.height, descriptor.fps
        )

    def push_video_frame(self, frame_data: bytes, timestamp_ns: int) -> None:
        self._native.push_video_frame(frame_data, timestamp_ns)

    def video_peer_count(self) -> int:
        return self._native.video_peer_count()

    # --- Events ---

    def emit_scene_snapshot(self, snapshot: dict[str, Any]) -> None:
        if self._delegate:
            self._delegate.on_scene_snapshot(snapshot)

    def emit_attribute_batch(self, batch: list[dict[str, Any]]) -> None:
        if self._delegate:
            self._delegate.on_attribute_batch(batch)

    def emit_mapping_event(self, event: dict[str, Any]) -> None:
        if self._delegate:
            self._delegate.on_mapping_event(event)

    def emit_mode_event(self, event: dict[str, Any]) -> None:
        if self._delegate:
            self._delegate.on_mode_event(event)

    def emit_client_event(self, event: dict[str, Any]) -> None:
        if self._delegate:
            self._delegate.on_client_event(event)

    def emit_recording_event(self, event: dict[str, Any]) -> None:
        if self._delegate:
            self._delegate.on_recording_event(event)

    def _start_event_pump(self) -> None:
        if self._event_thread is not None and self._event_thread.is_alive():
            return
        self._event_stop.clear()
        self._event_thread = threading.Thread(
            target=self._poll_client_events,
            name="motionstage-client-events",
            daemon=True,
        )
        self._event_thread.start()

    def _stop_event_pump(self) -> None:
        self._event_stop.set()
        if self._event_thread is not None:
            self._event_thread.join(timeout=1.0)
        self._event_thread = None
        self._known_clients.clear()
        self._known_mode = None
        self._known_attribute_values.clear()

    def _poll_client_events(self) -> None:
        next_sessions_at = time.monotonic()
        next_mode_at = next_sessions_at
        next_attributes_at = next_sessions_at
        while not self._event_stop.is_set():
            now = time.monotonic()
            if now >= next_sessions_at:
                try:
                    self._emit_client_deltas(self.sessions())
                except Exception:
                    LOGGER.exception("session catalog poll failed")
                next_sessions_at = now + self._session_poll_interval_s

            if now >= next_mode_at:
                try:
                    mode_changed = self._emit_mode_delta(self.mode())
                except Exception:
                    LOGGER.exception("mode poll failed")
                    mode_changed = False
                next_mode_at = now + self._mode_poll_interval_s
                if mode_changed:
                    next_attributes_at = now

            if now >= next_attributes_at:
                try:
                    self._emit_runtime_attribute_batch(self.runtime_attribute_values())
                except Exception:
                    LOGGER.exception("runtime attribute poll failed")
                next_attributes_at = now + self._current_attribute_poll_interval_s()

            next_due = min(next_sessions_at, next_mode_at, next_attributes_at)
            wait_for = max(0.0, next_due - time.monotonic())
            wait_for = min(wait_for, self._event_loop_tick_s)
            self._event_stop.wait(wait_for)

    def _emit_client_deltas(self, sessions: list[dict[str, Any]]) -> None:
        current: dict[str, tuple[str, tuple[str, ...], tuple[str, ...], tuple[str, ...], str]] = {}
        for session in sessions:
            device_id = str(session.get("device_id") or "").strip()
            if not device_id:
                continue

            state = str(session.get("state") or "").strip()
            if state.lower() == "closed":
                continue

            device_name = str(session.get("device_name") or device_id).strip() or device_id
            roles = tuple(
                sorted(str(value).strip() for value in (session.get("roles") or []) if str(value).strip())
            )
            features = tuple(
                sorted(str(value).strip() for value in (session.get("features") or []) if str(value).strip())
            )
            attributes = tuple(
                sorted(
                    str(value).strip()
                    for value in (session.get("advertised_attributes") or [])
                    if str(value).strip()
                )
            )
            current[device_id] = (device_name, roles, features, attributes, state)

            if self._known_clients.get(device_id) != current[device_id]:
                self.emit_client_event(
                    {
                        "kind": "upsert",
                        "device_id": device_id,
                        "device_name": device_name,
                        "roles": list(roles),
                        "features": list(features),
                        "advertised_attributes": list(attributes),
                        "state": state,
                        "session_id": session.get("session_id"),
                    }
                )

        for device_id in set(self._known_clients.keys()) - set(current.keys()):
            self.emit_client_event({"kind": "removed", "device_id": device_id})

        self._known_clients = current

    def _emit_mode_delta(self, mode: str) -> bool:
        normalized = str(mode).strip().lower()
        if not normalized or normalized == self._known_mode:
            return False
        self._known_mode = normalized
        self.emit_mode_event({"mode": normalized})
        return True

    def _current_attribute_poll_interval_s(self) -> float:
        if self._known_mode in {"live", "recording", "record", "playback", "play"}:
            return self._attribute_poll_interval_live_s
        return self._attribute_poll_interval_idle_s

    def _emit_runtime_attribute_batch(self, rows: list[BakeAttributeValue]) -> None:
        if not rows:
            return
        changed_batch: list[dict[str, Any]] = []
        next_values: dict[tuple[str, str], str] = {}
        for row in rows:
            object_id = row.object_id.strip()
            object_name = row.object_name.strip()
            attribute_name = row.attribute_name.strip()
            object_ref = object_id or object_name
            if not object_ref or not attribute_name:
                continue
            value = row.value
            signature = repr(value)
            key = (object_ref, attribute_name)
            next_values[key] = signature
            if self._known_attribute_values.get(key) != signature:
                changed_batch.append(
                    {
                        "object_id": object_id,
                        "object": object_name,
                        "attribute": attribute_name,
                        "value": value,
                    }
                )
        self._known_attribute_values = next_values
        if changed_batch:
            self.emit_attribute_batch(changed_batch)
