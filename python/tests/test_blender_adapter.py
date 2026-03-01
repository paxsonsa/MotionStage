from blender_adapter.motionstage_blender_adapter import BlenderAdapter, register_blender_delegate
from motionstage_sdk import MotionStageServer
import motionstage_sdk.server as server_module


class MockObject:
    def __init__(self):
        self.location = [0.0, 0.0, 0.0]


def test_attribute_batch_updates_mock_object_without_bpy():
    adapter = BlenderAdapter()
    obj = MockObject()
    adapter.object_cache["camera"] = obj

    adapter.on_attribute_batch(
        [
            {
                "object": "camera",
                "attribute": "position",
                "value": [1.0, 2.0, 3.0],
            }
        ]
    )

    assert obj.location == [1.0, 2.0, 3.0]
    assert adapter.events


def test_register_delegate_wires_server_to_adapter(monkeypatch):
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
            return (1, 2, 3, 4, 5, 6, 7)

        def start_recording(self, path: str) -> str:
            return "00000000-0000-0000-0000-000000000333"

        def stop_recording(self) -> None:
            return None

        def sessions(self):
            return []

        def create_mapping(self, request: dict[str, object]) -> str:
            return "00000000-0000-0000-0000-000000000123"

        def remove_mapping(self, mapping_id: str) -> None:
            return None

        def runtime_attribute_values(self):
            return []

    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _Native)
    server = MotionStageServer(name="test")
    adapter = BlenderAdapter()
    adapter.object_cache["camera"] = MockObject()
    register_blender_delegate(server, adapter)

    server.emit_attribute_batch(
        [{"object": "camera", "attribute": "position", "value": [4.0, 5.0, 6.0]}]
    )

    assert adapter.object_cache["camera"].location == [4.0, 5.0, 6.0]
