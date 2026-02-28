from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Optional
from uuid import UUID, uuid4

from .delegates import SceneUpdateDelegate

try:  # pragma: no cover - exercised only when extension module is present
    from cinemotion_sdk_rust import CineServer as _NativeCineServer
except Exception:  # pragma: no cover
    _NativeCineServer = None


class SecurityMode(str, Enum):
    TRUSTED_LAN = "trusted_lan"
    PAIRING_REQUIRED = "pairing_required"
    API_KEY = "api_key"
    API_KEY_PLUS_PAIRING = "api_key_plus_pairing"


@dataclass
class CineSession:
    device_id: UUID
    session_id: UUID
    state: str = "Active"


@dataclass
class SceneRegistry:
    _scenes: dict[UUID, dict[str, Any]] = field(default_factory=dict)

    def register_scene(self, descriptor: dict[str, Any]) -> UUID:
        scene_id = uuid4()
        self._scenes[scene_id] = descriptor
        return scene_id


@dataclass
class MappingManager:
    _mappings: dict[UUID, dict[str, Any]] = field(default_factory=dict)

    def create_mapping(self, request: dict[str, Any]) -> UUID:
        mapping_id = uuid4()
        self._mappings[mapping_id] = request
        return mapping_id

    def update_mapping(self, mapping_id: UUID, request: dict[str, Any]) -> None:
        if mapping_id not in self._mappings:
            raise KeyError("mapping not found")
        self._mappings[mapping_id] = request

    def remove_mapping(self, mapping_id: UUID) -> None:
        self._mappings.pop(mapping_id, None)


@dataclass
class RecordingController:
    is_recording: bool = False
    active_path: Optional[str] = None

    def start_recording(self, path: str) -> None:
        self.is_recording = True
        self.active_path = path

    def stop_recording(self) -> None:
        self.is_recording = False
        self.active_path = None


class CineServer:
    def __init__(self, name: str = "cinemotion", security: SecurityMode = SecurityMode.TRUSTED_LAN):
        self.name = name
        self.security = security
        self.scene_registry = SceneRegistry()
        self.mapping_manager = MappingManager()
        self.recording = RecordingController()
        self._delegate: Optional[SceneUpdateDelegate] = None
        self._native = _NativeCineServer(name=name) if _NativeCineServer is not None else None
        self._running = False

    def bind_delegate(self, delegate: SceneUpdateDelegate) -> None:
        self._delegate = delegate

    def start(self) -> str:
        self._running = True
        if self._native is not None:
            return self._native.start()
        return "0.0.0.0:7788"

    def stop(self) -> None:
        self._running = False
        if self._native is not None:
            self._native.stop()

    def create_default_scene(self, name: str) -> UUID:
        if self._native is not None:
            scene_id = self._native.create_default_scene(name)
            return UUID(scene_id)
        return self.scene_registry.register_scene({"name": name, "objects": ["camera"]})

    def set_live_mode(self) -> None:
        if self._native is not None:
            self._native.set_live_mode()

    def metrics(self) -> tuple[int, int, int, int, int, int, int]:
        if self._native is not None:
            return self._native.metrics()
        return (0, 0, 0, 0, 0, 0, 0)

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
