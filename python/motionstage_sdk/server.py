from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
import logging
import threading
import time
from typing import Any, Optional
from uuid import UUID

from .delegates import SceneUpdateDelegate

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

    def metrics(self) -> tuple[int, int, int, int, int, int, int]:
        return self._native.metrics()

    def start_recording(self, path: str) -> UUID:
        recording_id = self._native.start_recording(path)
        return UUID(str(recording_id))

    def stop_recording(self) -> None:
        self._native.stop_recording()

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

    def runtime_attribute_values(self) -> list[dict[str, Any]]:
        rows = self._native.runtime_attribute_values()
        values: list[dict[str, Any]] = []
        for row in rows:
            object_id = ""
            if isinstance(row, (list, tuple)) and len(row) == 4:
                object_id, object_name, attribute_name, value = row
            elif isinstance(row, (list, tuple)) and len(row) == 3:
                object_name, attribute_name, value = row
            else:
                continue
            values.append(
                {
                    "object_id": str(object_id).strip() if object_id is not None else "",
                    "object": str(object_name),
                    "attribute": str(attribute_name),
                    "value": value,
                }
            )
        return values

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
        if self._known_mode in {"live", "recording", "record"}:
            return self._attribute_poll_interval_live_s
        return self._attribute_poll_interval_idle_s

    def _emit_runtime_attribute_batch(self, rows: list[dict[str, Any]]) -> None:
        if not rows:
            return
        changed_batch: list[dict[str, Any]] = []
        next_values: dict[tuple[str, str], str] = {}
        for row in rows:
            object_id = str(row.get("object_id") or "").strip()
            object_name = str(row.get("object") or "").strip()
            attribute_name = str(row.get("attribute") or "").strip()
            object_ref = object_id or object_name
            if not object_ref or not attribute_name:
                continue
            value = row.get("value")
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
