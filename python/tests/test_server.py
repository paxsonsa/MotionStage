from __future__ import annotations

import pytest

import motionstage_sdk.server as server_module
from motionstage_sdk import MotionStageServer, SceneUpdateDelegate


class _Delegate(SceneUpdateDelegate):
    def __init__(self) -> None:
        self.calls: list[str] = []

    def on_scene_snapshot(self, snapshot: dict[str, object]) -> None:
        self.calls.append("scene")

    def on_attribute_batch(self, batch: list[dict[str, object]]) -> None:
        self.calls.append("attr")

    def on_mapping_event(self, event: dict[str, object]) -> None:
        self.calls.append("mapping")

    def on_mode_event(self, event: dict[str, object]) -> None:
        self.calls.append("mode")

    def on_client_event(self, event: dict[str, object]) -> None:
        self.calls.append("client")

    def on_recording_event(self, event: dict[str, object]) -> None:
        self.calls.append("record")


def test_server_requires_native_bridge(monkeypatch):
    monkeypatch.setattr(server_module, "_NativeMotionStageServer", None)

    with pytest.raises(RuntimeError, match="native extension is required"):
        server_module.MotionStageServer(name="unit")


def test_server_delegate_and_runtime_calls_with_native_bridge(monkeypatch):
    class _Native:
        def __init__(self, name: str | None = None):
            self._name = name or "motionstage"
            self._mode = "idle"
            self._allowlist: list[str] = []

        def start(self) -> str:
            return "127.0.0.1:9999"

        def stop(self) -> None:
            return None

        def upsert_scene(self, scene: dict[str, object]) -> str:
            assert scene["name"] == "shot"
            return "00000000-0000-0000-0000-000000000000"

        def set_active_scene(self, scene_id: str) -> None:
            assert scene_id == "00000000-0000-0000-0000-000000000000"

        def set_mode(self, mode: str) -> str:
            self._mode = mode
            return mode

        def mode(self) -> str:
            return self._mode

        def set_mode_control_allowlist(self, ids: list[str]) -> None:
            self._allowlist = list(ids)

        def mode_control_allowlist(self) -> list[str]:
            return list(self._allowlist)

        def metrics(self) -> tuple[int, int, int, int, int, int, int]:
            return (1, 2, 3, 4, 5, 6, 7)

        def start_recording(self, path: str) -> str:
            assert path == "/tmp/demo.cmtrk"
            return "00000000-0000-0000-0000-000000000333"

        def stop_recording(self) -> None:
            return None

        def sessions(self):
            return []

        def create_mapping(self, request: dict[str, object]) -> str:
            assert request["target_object_id"] == "00000000-0000-0000-0000-000000000222"
            return "00000000-0000-0000-0000-000000000123"

        def remove_mapping(self, mapping_id: str) -> None:
            assert mapping_id == "00000000-0000-0000-0000-000000000123"

        def reset_scene_to_baseline(self, scene_id: str | None) -> int:
            assert scene_id in {"00000000-0000-0000-0000-000000000000", None}
            return 2

        def commit_scene_baseline(self, scene_id: str | None) -> int:
            assert scene_id in {"00000000-0000-0000-0000-000000000000", None}
            return 3

        def commit_object_baseline(self, object_id: str, scene_id: str | None) -> int:
            assert object_id == "00000000-0000-0000-0000-000000000222"
            assert scene_id in {"00000000-0000-0000-0000-000000000000", None}
            return 4

        def runtime_attribute_values(self):
            return [
                (
                    "00000000-0000-0000-0000-000000000222",
                    "Camera",
                    "position",
                    [1.0, 2.0, 3.0],
                )
            ]

    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _Native)
    server = server_module.MotionStageServer(name="unit")
    delegate = _Delegate()
    server.bind_delegate(delegate)

    endpoint = server.start()
    scene_id = server.upsert_scene(
        {
            "id": "00000000-0000-0000-0000-000000000000",
            "name": "shot",
            "objects": [],
        }
    )
    server.set_active_scene(scene_id)
    assert server.set_mode("live") == "live"
    server.set_mode_control_allowlist(
        [server_module.UUID("00000000-0000-0000-0000-000000000999")]
    )
    assert server.mode_control_allowlist() == [
        server_module.UUID("00000000-0000-0000-0000-000000000999")
    ]
    metrics = server.metrics()
    recording_id = server.start_recording("/tmp/demo.cmtrk")
    assert str(recording_id) == "00000000-0000-0000-0000-000000000333"
    server.stop_recording()
    mapping_id = server.create_mapping(
        {
            "source_device": "00000000-0000-0000-0000-000000000111",
            "source_output": "demo.position",
            "target_scene": "00000000-0000-0000-0000-000000000000",
            "target_object_id": "00000000-0000-0000-0000-000000000222",
            "target_attribute": "position",
            "component_mask": [0, 1, 2],
        }
    )
    server.remove_mapping(mapping_id)
    assert (
        server.reset_scene_to_baseline(
            server_module.UUID("00000000-0000-0000-0000-000000000000")
        )
        == 2
    )
    assert server.commit_scene_baseline() == 3
    assert (
        server.commit_object_baseline(
            server_module.UUID("00000000-0000-0000-0000-000000000222"),
            server_module.UUID("00000000-0000-0000-0000-000000000000"),
        )
        == 4
    )
    rows = server.runtime_attribute_values()
    assert rows[0]["object_id"] == "00000000-0000-0000-0000-000000000222"
    assert rows[0]["object"] == "Camera"

    server.emit_scene_snapshot({"name": "shot"})
    server.emit_attribute_batch([{"object": "camera", "attribute": "position", "value": [1, 2, 3]}])
    server.emit_mapping_event({"kind": "created"})
    server.emit_mode_event({"mode": "Live"})
    server.emit_client_event({"device": "ipad"})
    server.emit_recording_event({"state": "started"})
    server.stop()

    assert ":" in endpoint
    assert str(scene_id)
    assert len(metrics) == 7
    for expected in ("scene", "attr", "mapping", "mode", "client", "record"):
        assert expected in delegate.calls


def test_server_emits_client_event_deltas_from_session_snapshots(monkeypatch):
    class _Native:
        def __init__(self, name: str | None = None):
            self._name = name or "motionstage"

        def start(self) -> str:
            return "127.0.0.1:9999"

        def stop(self) -> None:
            return None

        def upsert_scene(self, scene: dict[str, object]) -> str:
            return "00000000-0000-0000-0000-000000000000"

        def set_active_scene(self, scene_id: str) -> None:
            return None

        def set_mode(self, mode: str) -> str:
            return mode

        def mode(self) -> str:
            return "idle"

        def set_mode_control_allowlist(self, ids: list[str]) -> None:
            return None

        def mode_control_allowlist(self) -> list[str]:
            return []

        def metrics(self) -> tuple[int, int, int, int, int, int, int]:
            return (0, 0, 0, 0, 0, 0, 0)

        def start_recording(self, path: str) -> str:
            return "00000000-0000-0000-0000-000000000001"

        def stop_recording(self) -> None:
            return None

        def sessions(self):
            return []

        def create_mapping(self, request: dict[str, object]) -> str:
            return "00000000-0000-0000-0000-000000000010"

        def remove_mapping(self, mapping_id: str) -> None:
            return None

        def runtime_attribute_values(self):
            return []

    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _Native)
    server = server_module.MotionStageServer(name="unit")
    delegate = _Delegate()
    server.bind_delegate(delegate)

    server._emit_client_deltas(  # type: ignore[attr-defined]
        [
            {
                "device_id": "ipad-1",
                "device_name": "iPad Pro",
                "session_id": "00000000-0000-0000-0000-000000000123",
                "roles": ["operator"],
                "features": ["mapping"],
                "advertised_attributes": ["demo.position"],
                "state": "Active",
            }
        ]
    )

    assert "client" in delegate.calls


def test_closed_session_emits_client_removed_delta(monkeypatch):
    class _Native:
        def __init__(self, name: str | None = None):
            self._name = name or "motionstage"

        def start(self) -> str:
            return "127.0.0.1:9999"

        def stop(self) -> None:
            return None

        def upsert_scene(self, scene: dict[str, object]) -> str:
            return "00000000-0000-0000-0000-000000000000"

        def set_active_scene(self, scene_id: str) -> None:
            return None

        def set_mode(self, mode: str) -> str:
            return mode

        def mode(self) -> str:
            return "idle"

        def set_mode_control_allowlist(self, ids: list[str]) -> None:
            return None

        def mode_control_allowlist(self) -> list[str]:
            return []

        def metrics(self) -> tuple[int, int, int, int, int, int, int]:
            return (0, 0, 0, 0, 0, 0, 0)

        def start_recording(self, path: str) -> str:
            return "00000000-0000-0000-0000-000000000001"

        def stop_recording(self) -> None:
            return None

        def sessions(self):
            return []

        def create_mapping(self, request: dict[str, object]) -> str:
            return "00000000-0000-0000-0000-000000000010"

        def remove_mapping(self, mapping_id: str) -> None:
            return None

        def runtime_attribute_values(self):
            return []

    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _Native)
    server = server_module.MotionStageServer(name="unit")
    captured: list[dict[str, object]] = []
    server.bind_delegate(_Delegate())
    server.emit_client_event = captured.append  # type: ignore[method-assign]

    server._emit_client_deltas(  # type: ignore[attr-defined]
        [
            {
                "device_id": "ipad-1",
                "device_name": "iPad Pro",
                "roles": ["operator"],
                "features": ["mapping"],
                "advertised_attributes": ["demo.position"],
                "state": "Active",
            }
        ]
    )
    server._emit_client_deltas(  # type: ignore[attr-defined]
        [
            {
                "device_id": "ipad-1",
                "device_name": "iPad Pro",
                "roles": ["operator"],
                "features": ["mapping"],
                "advertised_attributes": ["demo.position"],
                "state": "Closed",
            }
        ]
    )

    assert captured[0]["kind"] == "upsert"
    assert captured[-1]["kind"] == "removed"


def test_runtime_attribute_values_accepts_legacy_row_shape(monkeypatch):
    class _Native:
        def __init__(self, name: str | None = None):
            self._name = name or "motionstage"

        def start(self) -> str:
            return "127.0.0.1:9999"

        def stop(self) -> None:
            return None

        def upsert_scene(self, scene: dict[str, object]) -> str:
            return "00000000-0000-0000-0000-000000000000"

        def set_active_scene(self, scene_id: str) -> None:
            return None

        def set_mode(self, mode: str) -> str:
            return mode

        def mode(self) -> str:
            return "idle"

        def set_mode_control_allowlist(self, ids: list[str]) -> None:
            return None

        def mode_control_allowlist(self) -> list[str]:
            return []

        def metrics(self) -> tuple[int, int, int, int, int, int, int]:
            return (0, 0, 0, 0, 0, 0, 0)

        def start_recording(self, path: str) -> str:
            return "00000000-0000-0000-0000-000000000001"

        def stop_recording(self) -> None:
            return None

        def sessions(self):
            return []

        def create_mapping(self, request: dict[str, object]) -> str:
            return "00000000-0000-0000-0000-000000000010"

        def remove_mapping(self, mapping_id: str) -> None:
            return None

        def runtime_attribute_values(self):
            return [("Camera", "position", [1.0, 2.0, 3.0])]

    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _Native)
    server = server_module.MotionStageServer(name="unit")
    rows = server.runtime_attribute_values()
    assert rows == [
        {
            "object_id": "",
            "object": "Camera",
            "attribute": "position",
            "value": [1.0, 2.0, 3.0],
        }
    ]
