from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from motionstage_sdk.delegates import SceneUpdateDelegate

try:
    import bpy  # type: ignore
except Exception:  # pragma: no cover
    bpy = None


@dataclass
class BlenderAdapter(SceneUpdateDelegate):
    """Reference Blender adapter wiring MotionStage callbacks to scene updates."""

    object_cache: dict[str, Any] = field(default_factory=dict)
    events: list[dict[str, Any]] = field(default_factory=list)

    def _resolve_object(self, name: str):
        if bpy is None:
            return self.object_cache.get(name)
        obj = bpy.data.objects.get(name)
        if obj is not None:
            self.object_cache[name] = obj
        return obj

    def on_scene_snapshot(self, snapshot: dict[str, Any]) -> None:
        self.events.append({"kind": "scene_snapshot", "snapshot": snapshot})

    def on_attribute_batch(self, batch: list[dict[str, Any]]) -> None:
        self.events.append({"kind": "attribute_batch", "batch": batch})
        for event in batch:
            object_name = event.get("object")
            attribute_name = event.get("attribute")
            value = event.get("value")
            obj = self._resolve_object(object_name)
            if obj is None:
                continue
            if attribute_name == "position" and hasattr(obj, "location") and isinstance(value, (list, tuple)):
                obj.location[0] = value[0]
                obj.location[1] = value[1]
                obj.location[2] = value[2]

    def on_mapping_event(self, event: dict[str, Any]) -> None:
        self.events.append({"kind": "mapping_event", "event": event})

    def on_mode_event(self, event: dict[str, Any]) -> None:
        self.events.append({"kind": "mode_event", "event": event})

    def on_client_event(self, event: dict[str, Any]) -> None:
        self.events.append({"kind": "client_event", "event": event})

    def on_recording_event(self, event: dict[str, Any]) -> None:
        self.events.append({"kind": "recording_event", "event": event})


def register_blender_delegate(server, adapter: BlenderAdapter | None = None) -> BlenderAdapter:
    adapter = adapter or BlenderAdapter()
    server.bind_delegate(adapter)
    return adapter
