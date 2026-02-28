from __future__ import annotations

import cinemotion_sdk.server as server_module
from cinemotion_sdk import CineServer, SceneUpdateDelegate


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


def test_server_delegate_and_fallback_runtime_calls():
    server = CineServer(name="unit")
    delegate = _Delegate()
    server.bind_delegate(delegate)

    endpoint = server.start()
    scene_id = server.create_default_scene("shot")
    server.set_live_mode()
    metrics = server.metrics()
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
    assert delegate.calls == ["scene", "attr", "mapping", "mode", "client", "record"]


def test_server_uses_native_bridge_when_present(monkeypatch):
    class _Native:
        def __init__(self, name: str | None = None):
            self._name = name or "cinemotion"

        def start(self) -> str:
            return "127.0.0.1:9999"

        def stop(self) -> None:
            return None

        def create_default_scene(self, name: str) -> str:
            return "00000000-0000-0000-0000-000000000000"

        def set_live_mode(self) -> None:
            return None

        def metrics(self) -> tuple[int, int, int, int, int, int, int]:
            return (1, 2, 3, 4, 5, 6, 7)

    monkeypatch.setattr(server_module, "_NativeCineServer", _Native)
    native_server = server_module.CineServer(name="native")
    assert native_server.start() == "127.0.0.1:9999"
    assert native_server.metrics() == (1, 2, 3, 4, 5, 6, 7)
