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
    is_host: bool = False


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
        # Attribute values are the data plane: the core emits no per-value
        # events, so the pump samples them at a fixed high-performance cadence
        # (fast while data flows, slow while idle). Everything else is
        # event-driven via drain_state_events.
        self._attribute_poll_interval_live_s = 1.0 / 120.0
        self._attribute_poll_interval_idle_s = 0.25
        self._event_thread: Optional[threading.Thread] = None
        self._event_stop = threading.Event()
        # Diff cache for the attribute-value poll (data plane only).
        self._known_attribute_values: dict[tuple[str, str], str] = {}
        # Mode as last reported by the event stream / snapshot; drives the
        # attribute poll cadence.
        self._current_mode: Optional[str] = None
        # session_id -> (device_id, device_name), learned from session_joined
        # events so session_left can carry the device identity too.
        self._session_devices: dict[str, tuple[str, str]] = {}
        # The in-process host session (the DCC itself). It rides the event
        # plane like any session but is not a motion client: it is excluded
        # from the client catalog and from on_client_event deltas.
        try:
            self._host_session_id: str = str(self._native.host_session_id())
        except Exception:  # pragma: no cover - bridge predates host sessions
            self._host_session_id = ""
        # Monotonic deadline of the next attribute-value poll; event dispatch
        # pulls it forward when server state invalidates cached values.
        self._next_attributes_at = 0.0

    def bind_delegate(self, delegate: SceneUpdateDelegate) -> None:
        pump_was_running = self._event_thread is not None and self._event_thread.is_alive()
        self._delegate = delegate
        if self._running:
            self._start_event_pump()
            if pump_was_running:
                # The pump only emits the snapshot once at startup; a delegate
                # bound later still needs the current world state.
                try:
                    self._emit_scene_snapshot_from_native()
                except Exception:
                    LOGGER.exception("scene snapshot emit on delegate bind failed")

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
        """The client catalog: registered motion clients only.

        The native bridge reports every session record (all lifecycle states,
        the in-process host session included). Consumers of this method build
        their client/source catalogs from it, so it drops:

        * the host session (``is_host`` — the DCC itself is not a client), and
        * pre-registration sessions (no ``session_id`` yet) — they never emit
          ``session_joined``/``session_left``, so listing them would create
          entries no event could ever remove.
        """
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
                is_host,
            ) = row
            if bool(is_host) or not session_id:
                continue
            if str(state).strip().lower() == "closed":
                continue
            sessions.append(
                {
                    "device_id": str(device_id),
                    "device_name": str(device_name),
                    "session_id": str(session_id),
                    "roles": [str(value) for value in roles],
                    "features": [str(value) for value in features],
                    "advertised_attributes": [str(value) for value in advertised_attributes],
                    "state": str(state),
                    "is_host": False,
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

    def push_video_frame_bgra(self, frame_data: bytes, timestamp_ns: int) -> None:
        self._native.push_video_frame_bgra(frame_data, timestamp_ns)

    def video_peer_count(self) -> int:
        return self._native.video_peer_count()

    # --- Companion UI ---

    def start_companion_ui(self) -> int:
        """Start the embedded companion-UI listener (idempotent); return its port."""
        return int(self._native.start_companion_ui())

    def companion_ui_token(self) -> str | None:
        """Auth token carried in the companion-UI URL, or None if not started."""
        token = self._native.companion_ui_token()
        return str(token) if token is not None else None

    def companion_ui_url(self) -> str:
        """Token-bearing localhost URL for opening the companion UI in a browser."""
        port = self.start_companion_ui()
        token = self.companion_ui_token()
        base = f"http://127.0.0.1:{port}/"
        return f"{base}?token={token}" if token else base

    def stop_companion_ui(self) -> None:
        """Gracefully stop the companion-UI listener if running."""
        self._native.stop_companion_ui()

    def drain_host_requests(self) -> list[dict[str, Any]]:
        """DCC-side actions the companion UI requested, for the host to execute on
        its main thread. Each dict has a ``kind`` discriminator (resync_scene,
        start_video, stop_video, bake_take)."""
        return list(self._native.drain_host_requests())

    def set_host_selection(self, names: list[str]) -> None:
        """Report objects selected in the host DCC (by name) for UI highlight."""
        self._native.set_host_selection([str(n) for n in names])

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
            target=self._event_pump,
            name="motionstage-state-events",
            daemon=True,
        )
        self._event_thread.start()

    def _stop_event_pump(self) -> None:
        self._event_stop.set()
        if self._event_thread is not None:
            self._event_thread.join(timeout=1.0)
        self._event_thread = None
        self._known_attribute_values.clear()
        self._session_devices.clear()
        self._current_mode = None

    def _event_pump(self) -> None:
        """Single background loop: replicated state events drive the control
        plane callbacks; attribute values (the data plane, which has no core
        event) are sampled inside the same loop at a mode-dependent cadence."""
        drain = getattr(self._native, "drain_state_events", None)
        try:
            self._emit_scene_snapshot_from_native()
            if self._current_mode:
                self.emit_mode_event(
                    {"type": "mode_changed", "mode": self._current_mode, "initial": True}
                )
        except Exception:
            LOGGER.exception("initial scene snapshot failed")
        if drain is None:
            LOGGER.error(
                "native bridge lacks drain_state_events(); state events disabled, "
                "attribute polling only (rebuild the motionstage_sdk_rust extension)"
            )

        self._next_attributes_at = time.monotonic()
        while not self._event_stop.is_set():
            budget_s = max(0.0, self._next_attributes_at - time.monotonic())
            # Never block past the next attribute-poll deadline, and cap the
            # wait so stop() stays responsive.
            wait_s = min(budget_s, self._attribute_poll_interval_idle_s)
            if drain is not None:
                timeout_ms = int(wait_s * 1000.0)
                if timeout_ms == 0 and budget_s > 0.0:
                    timeout_ms = 1
                try:
                    events = drain(timeout_ms)
                except Exception:
                    LOGGER.exception("state event drain failed")
                    events = []
                    self._event_stop.wait(0.1)
                for event in events:
                    if self._event_stop.is_set():
                        break
                    try:
                        self._dispatch_state_event(event)
                    except Exception:
                        LOGGER.exception("state event dispatch failed")
            elif wait_s > 0.0:
                self._event_stop.wait(wait_s)

            if self._event_stop.is_set():
                break
            now = time.monotonic()
            if now >= self._next_attributes_at:
                try:
                    self._emit_runtime_attribute_batch(self.runtime_attribute_values())
                except Exception:
                    LOGGER.exception("runtime attribute poll failed")
                self._next_attributes_at = (
                    time.monotonic() + self._current_attribute_poll_interval_s()
                )

    # State-event -> delegate-callback routing tables. Values are the short
    # `kind` discriminator carried alongside the raw event payload.
    _MAPPING_EVENT_KINDS = {
        "mapping_created": "created",
        "mapping_updated": "updated",
        "mapping_removed": "removed",
        "mapping_lock_changed": "lock_changed",
        "mapping_released": "released",
    }
    _RECORDING_EVENT_TYPES = frozenset(
        {
            "recording_started",
            "recording_stopped",
            "take_registered",
            "take_selected",
            "take_deleted",
            "playback_changed",
        }
    )

    def _dispatch_state_event(self, event: dict[str, Any]) -> None:
        event_type = str(event.get("type") or "").strip()

        if event_type == "mode_changed":
            mode = str(event.get("mode") or "").strip().lower()
            if mode:
                self._current_mode = mode
                # Mode transitions change what flows; refresh values promptly.
                self._next_attributes_at = 0.0
            self.emit_mode_event(dict(event))
            return

        if event_type in self._MAPPING_EVENT_KINDS:
            enriched = dict(event)
            enriched["kind"] = self._MAPPING_EVENT_KINDS[event_type]
            self.emit_mapping_event(enriched)
            return

        if event_type == "session_joined":
            session_id = str(event.get("session_id") or "").strip()
            # The in-process host session is not a motion client; its
            # join/leave never reaches on_client_event.
            if session_id and session_id == self._host_session_id:
                return
            device_id = str(event.get("device_id") or "").strip()
            device_name = str(event.get("device_name") or "").strip() or device_id
            if session_id:
                self._session_devices[session_id] = (device_id, device_name)
            enriched = dict(event)
            enriched["kind"] = "upsert"
            enriched.setdefault("state", "Active")
            self.emit_client_event(enriched)
            return

        if event_type == "session_left":
            session_id = str(event.get("session_id") or "").strip()
            if session_id and session_id == self._host_session_id:
                return
            device_id, device_name = self._session_devices.pop(session_id, ("", ""))
            enriched = dict(event)
            enriched["kind"] = "removed"
            if device_id:
                enriched.setdefault("device_id", device_id)
                enriched.setdefault("device_name", device_name)
            self.emit_client_event(enriched)
            return

        if event_type in self._RECORDING_EVENT_TYPES:
            enriched = dict(event)
            enriched["kind"] = event_type
            self.emit_recording_event(enriched)
            return

        if event_type in {"scene_loaded", "scene_activated", "event_stream_lagged"}:
            # Scene graph changed (or we fell behind the stream): re-baseline
            # from a fresh snapshot and invalidate the attribute diff cache.
            self._known_attribute_values.clear()
            self._next_attributes_at = 0.0
            try:
                self._emit_scene_snapshot_from_native()
            except Exception:
                LOGGER.exception("scene snapshot refresh failed")
            return

        if event_type == "baseline_applied":
            # Attribute values were rewritten wholesale; poll immediately.
            self._next_attributes_at = 0.0
            return

        LOGGER.debug("ignoring unhandled state event type: %s", event_type)

    def _emit_scene_snapshot_from_native(self) -> None:
        fetch = getattr(self._native, "initial_scene_snapshot", None)
        if fetch is None:
            return
        snapshot = fetch()
        mode = str(snapshot.get("mode") or "").strip().lower()
        if mode:
            self._current_mode = mode
        self.emit_scene_snapshot(dict(snapshot))

    def _current_attribute_poll_interval_s(self) -> float:
        if self._current_mode in {"live", "recording", "playback"}:
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
