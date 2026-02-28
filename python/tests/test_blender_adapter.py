from blender_adapter.motionstage_blender_adapter import BlenderAdapter, register_blender_delegate
from motionstage_sdk import MotionStageServer


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


def test_register_delegate_wires_server_to_adapter():
    server = MotionStageServer(name="test")
    adapter = BlenderAdapter()
    adapter.object_cache["camera"] = MockObject()
    register_blender_delegate(server, adapter)

    server.emit_attribute_batch(
        [{"object": "camera", "attribute": "position", "value": [4.0, 5.0, 6.0]}]
    )

    assert adapter.object_cache["camera"].location == [4.0, 5.0, 6.0]
