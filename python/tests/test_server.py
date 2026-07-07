from __future__ import annotations

import threading
import time

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


_ALL_CALLBACKS = frozenset({"scene", "attr", "mapping", "mode", "client", "record"})


class _CapturingDelegate(SceneUpdateDelegate):
    """Thread-safe delegate that records payloads and signals completion once
    every callback kind has fired at least once."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.by_kind: dict[str, list[object]] = {kind: [] for kind in _ALL_CALLBACKS}
        self.all_fired = threading.Event()

    def _record(self, kind: str, payload: object) -> None:
        with self._lock:
            self.by_kind[kind].append(payload)
            if all(self.by_kind[k] for k in _ALL_CALLBACKS):
                self.all_fired.set()

    def on_scene_snapshot(self, snapshot: dict[str, object]) -> None:
        self._record("scene", snapshot)

    def on_attribute_batch(self, batch: list[dict[str, object]]) -> None:
        self._record("attr", batch)

    def on_mapping_event(self, event: dict[str, object]) -> None:
        self._record("mapping", event)

    def on_mode_event(self, event: dict[str, object]) -> None:
        self._record("mode", event)

    def on_client_event(self, event: dict[str, object]) -> None:
        self._record("client", event)

    def on_recording_event(self, event: dict[str, object]) -> None:
        self._record("record", event)


class _FakeNative:
    """Fake pyo3 bridge mirroring the documented drain/snapshot event API."""

    def __init__(self, name: str | None = None):
        self._name = name or "motionstage"
        self._mode = "idle"
        # Batches handed out one per drain_state_events call.
        self.event_batches: list[list[dict[str, object]]] = []
        self.snapshot: dict[str, object] = {
            "seq": 0,
            "mode": "idle",
            "data_flow": "idle",
            "recording": "inactive",
            "active_scene": None,
            "scenes": [],
            "mappings": [],
            "sessions": [],
            "takes": [],
            "playback": None,
        }
        self.drain_calls = 0
        self.snapshot_calls = 0

    def start(self) -> str:
        return "127.0.0.1:9999"

    def stop(self) -> None:
        return None

    def host_session_id(self) -> str:
        return "00000000-0000-0000-0000-00000000host"

    def host_device_id(self) -> str:
        return "00000000-0000-0000-0000-0000000hostd"

    def initial_scene_snapshot(self) -> dict[str, object]:
        self.snapshot_calls += 1
        return dict(self.snapshot)

    def drain_state_events(self, timeout_ms: int = 0) -> list[dict[str, object]]:
        self.drain_calls += 1
        if self.event_batches:
            return self.event_batches.pop(0)
        if timeout_ms:
            time.sleep(min(timeout_ms, 10) / 1000.0)
        return []

    def upsert_scene(self, scene: dict[str, object]) -> str:
        return "00000000-0000-0000-0000-000000000000"

    def set_active_scene(self, scene_id: str) -> None:
        return None

    def set_mode(self, mode: str) -> str:
        self._mode = mode
        return mode

    def mode(self) -> str:
        return self._mode

    def set_data_flow(self, live: bool) -> str:
        self._mode = "live" if live else "idle"
        return self._mode

    def set_recording(self, recording: bool) -> str:
        self._mode = "recording" if recording else "live"
        return self._mode

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

    def update_mapping(self, mapping_id: str, request: dict[str, object]) -> None:
        return None

    def remove_mapping(self, mapping_id: str) -> None:
        return None

    def set_mapping_lock(self, mapping_id: str, lock: bool) -> None:
        return None

    def runtime_attribute_values(self):
        return []


def test_server_requires_native_bridge(monkeypatch):
    monkeypatch.setattr(server_module, "_NativeMotionStageServer", None)

    with pytest.raises(RuntimeError, match="native extension is required"):
        server_module.MotionStageServer(name="unit")


def test_server_delegate_and_runtime_calls_with_native_bridge(monkeypatch):
    class _Native(_FakeNative):
        def upsert_scene(self, scene: dict[str, object]) -> str:
            assert scene["name"] == "shot"
            return "00000000-0000-0000-0000-000000000000"

        def set_active_scene(self, scene_id: str) -> None:
            assert scene_id == "00000000-0000-0000-0000-000000000000"

        def metrics(self) -> tuple[int, int, int, int, int, int, int]:
            return (1, 2, 3, 4, 5, 6, 7)

        def start_recording(self, path: str) -> str:
            assert path == "/tmp/demo.cmtrk"
            return "00000000-0000-0000-0000-000000000333"

        def create_mapping(self, request: dict[str, object]) -> str:
            assert request["target_object_id"] == "00000000-0000-0000-0000-000000000222"
            return "00000000-0000-0000-0000-000000000123"

        def update_mapping(self, mapping_id: str, request: dict[str, object]) -> None:
            assert mapping_id == "00000000-0000-0000-0000-000000000123"
            assert request["source_device"] == "00000000-0000-0000-0000-000000000111"
            assert request["target_object_id"] == "00000000-0000-0000-0000-000000000222"
            assert request["component_mask"] == [0]

        def remove_mapping(self, mapping_id: str) -> None:
            assert mapping_id == "00000000-0000-0000-0000-000000000123"

        def set_mapping_lock(self, mapping_id: str, lock: bool) -> None:
            assert mapping_id == "00000000-0000-0000-0000-000000000123"
            assert lock is True

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
    assert server.set_data_flow(True) == "live"
    assert server.set_recording(True) == "recording"
    assert server.set_recording(False) == "live"
    assert server.set_data_flow(False) == "idle"
    metrics = server.metrics()
    assert isinstance(metrics, server_module.ServerMetrics)
    assert metrics.accepted_sessions == 1
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
    server.update_mapping(
        mapping_id,
        {
            "source_device": server_module.UUID("00000000-0000-0000-0000-000000000111"),
            "source_output": "demo.position",
            "target_object_id": server_module.UUID("00000000-0000-0000-0000-000000000222"),
            "target_attribute": "position",
            "component_mask": [0],
        },
    )
    server.set_mapping_lock(mapping_id, True)
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
    assert rows[0].object_id == "00000000-0000-0000-0000-000000000222"
    assert rows[0].object_name == "Camera"

    server.emit_scene_snapshot({"name": "shot"})
    server.emit_attribute_batch([{"object": "camera", "attribute": "position", "value": [1, 2, 3]}])
    server.emit_mapping_event({"kind": "created"})
    server.emit_mode_event({"mode": "Live"})
    server.emit_client_event({"device": "ipad"})
    server.emit_recording_event({"state": "started"})
    server.stop()

    assert ":" in endpoint
    assert str(scene_id)
    assert isinstance(metrics, server_module.ServerMetrics)
    for expected in ("scene", "attr", "mapping", "mode", "client", "record"):
        assert expected in delegate.calls


def test_event_pump_fires_all_six_delegate_callbacks(monkeypatch):
    """One drain-driven pump translates state events into every delegate
    callback: snapshot at start, then mode/mapping/client/recording events,
    plus the attribute-value data-plane batch."""

    class _Native(_FakeNative):
        def __init__(self, name: str | None = None):
            super().__init__(name)
            self.event_batches = [
                [
                    {
                        "seq": 1,
                        "origin_session": "00000000-0000-0000-0000-000000000aaa",
                        "timestamp_ns": 10,
                        "type": "mode_changed",
                        "mode": "live",
                        "data_flow": "live",
                        "recording": "inactive",
                    },
                    {
                        "seq": 2,
                        "origin_session": "00000000-0000-0000-0000-000000000aaa",
                        "timestamp_ns": 20,
                        "type": "mapping_created",
                        "mapping": {
                            "mapping_id": "00000000-0000-0000-0000-000000000123",
                            "source_device": "00000000-0000-0000-0000-000000000111",
                            "source_output": "demo.position",
                            "target_scene": "00000000-0000-0000-0000-000000000000",
                            "target_object": "00000000-0000-0000-0000-000000000222",
                            "target_attribute": "position",
                            "component_mask": None,
                            "lock": False,
                        },
                    },
                ],
                [
                    {
                        "seq": 3,
                        "origin_session": None,
                        "timestamp_ns": 30,
                        "type": "session_joined",
                        "session_id": "00000000-0000-0000-0000-000000000bbb",
                        "device_id": "00000000-0000-0000-0000-000000000ccc",
                        "device_name": "iPad Pro",
                        "roles": ["operator"],
                    },
                    {
                        "seq": 4,
                        "origin_session": None,
                        "timestamp_ns": 40,
                        "type": "recording_started",
                        "take_id": "00000000-0000-0000-0000-000000000ddd",
                        "scene_id": "00000000-0000-0000-0000-000000000000",
                    },
                ],
            ]

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
    delegate = _CapturingDelegate()
    server.bind_delegate(delegate)

    server.start()
    try:
        assert delegate.all_fired.wait(timeout=5.0), (
            "missing callbacks: "
            + ", ".join(sorted(k for k, v in delegate.by_kind.items() if not v))
        )
    finally:
        server.stop()

    snapshot = delegate.by_kind["scene"][0]
    assert snapshot["mode"] == "idle"
    assert snapshot["scenes"] == []

    mode_events = delegate.by_kind["mode"]
    assert any(event.get("mode") == "live" for event in mode_events)

    mapping_event = delegate.by_kind["mapping"][0]
    assert mapping_event["kind"] == "created"
    assert mapping_event["mapping"]["target_attribute"] == "position"

    client_event = delegate.by_kind["client"][0]
    assert client_event["kind"] == "upsert"
    assert client_event["device_id"] == "00000000-0000-0000-0000-000000000ccc"
    assert client_event["device_name"] == "iPad Pro"

    recording_event = delegate.by_kind["record"][0]
    assert recording_event["kind"] == "recording_started"
    assert recording_event["take_id"] == "00000000-0000-0000-0000-000000000ddd"

    batch = delegate.by_kind["attr"][0]
    assert batch == [
        {
            "object_id": "00000000-0000-0000-0000-000000000222",
            "object": "Camera",
            "attribute": "position",
            "value": [1.0, 2.0, 3.0],
        }
    ]


def test_pump_alive_transitions_with_start_and_stop(monkeypatch):
    """pump_alive() is the public health hook: False before start, True while
    the drain thread runs, False again after stop() joins it."""
    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _FakeNative)
    server = server_module.MotionStageServer(name="unit")

    assert server.pump_alive() is False
    assert server.last_event_monotonic() is None

    server.start()
    try:
        assert server.pump_alive() is True
    finally:
        server.stop()

    assert server.pump_alive() is False
    # stop() joins and resets the pump; the last-event clock resets too.
    assert server.last_event_monotonic() is None


def test_delegate_event_dicts_carry_monotonic_seq(monkeypatch):
    """Every event dict handed to the on_*_event delegate callbacks carries the
    envelope's monotonic seq (plus origin_session/timestamp_ns), so a consumer
    can guard against stale snapshots reordering registrations."""

    class _Native(_FakeNative):
        def __init__(self, name: str | None = None):
            super().__init__(name)
            self.event_batches = [
                [
                    {
                        "seq": 1,
                        "origin_session": "00000000-0000-0000-0000-000000000aaa",
                        "timestamp_ns": 10,
                        "type": "mode_changed",
                        "mode": "live",
                    },
                    {
                        "seq": 2,
                        "origin_session": "00000000-0000-0000-0000-000000000aaa",
                        "timestamp_ns": 20,
                        "type": "mapping_created",
                        "mapping": {"mapping_id": "00000000-0000-0000-0000-000000000123"},
                    },
                ],
                [
                    {
                        "seq": 3,
                        "origin_session": None,
                        "timestamp_ns": 30,
                        "type": "session_joined",
                        "session_id": "00000000-0000-0000-0000-000000000bbb",
                        "device_id": "00000000-0000-0000-0000-000000000ccc",
                        "device_name": "iPad Pro",
                    },
                    {
                        "seq": 4,
                        "origin_session": None,
                        "timestamp_ns": 40,
                        "type": "recording_started",
                        "take_id": "00000000-0000-0000-0000-000000000ddd",
                        "scene_id": "00000000-0000-0000-0000-000000000000",
                    },
                ],
            ]

    class _SeqDelegate(_CapturingDelegate):
        def __init__(self) -> None:
            super().__init__()
            self.saw_recording = threading.Event()

        def on_recording_event(self, event: dict[str, object]) -> None:
            super().on_recording_event(event)
            self.saw_recording.set()

    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _Native)
    server = server_module.MotionStageServer(name="unit")
    delegate = _SeqDelegate()
    server.bind_delegate(delegate)

    server.start()
    try:
        assert delegate.saw_recording.wait(timeout=5.0), "recording event never delivered"
    finally:
        server.stop()

    # Collect every event dict delivered to the control-plane callbacks, in
    # delivery order (mode -> mapping -> client -> recording).
    delivered = (
        delegate.by_kind["mode"]
        + delegate.by_kind["mapping"]
        + delegate.by_kind["client"]
        + delegate.by_kind["record"]
    )
    assert delivered, "no control-plane events were delivered"
    for event in delivered:
        assert "seq" in event, f"event missing seq: {event}"
        assert "timestamp_ns" in event
        assert "origin_session" in event

    # In the order the pump dispatched them, seq is strictly increasing.
    seqs = [
        int(delegate.by_kind["mode"][0]["seq"]),
        int(delegate.by_kind["mapping"][0]["seq"]),
        int(delegate.by_kind["client"][0]["seq"]),
        int(delegate.by_kind["record"][0]["seq"]),
    ]
    assert seqs == sorted(seqs)
    assert len(set(seqs)) == len(seqs)
    assert server.last_event_monotonic() is None  # reset by stop()


def test_sessions_catalog_excludes_host_and_unregistered_rows(monkeypatch):
    """server.sessions() is the client catalog: the in-process host session and
    sessions that never registered (no session_id) or already closed must not
    appear, or SDK consumers gain phantom clients no event will ever remove."""

    class _Native(_FakeNative):
        def sessions(self):
            return [
                # The in-process host session: registered, but not a client.
                (
                    "00000000-0000-0000-0000-000000000h0d",
                    "host",
                    "00000000-0000-0000-0000-00000000host",
                    ["scene_author", "operator"],
                    ["motion", "mapping"],
                    [],
                    "Active",
                    True,
                ),
                # A registered device client: the only catalog entry.
                (
                    "00000000-0000-0000-0000-000000000ccc",
                    "iPad Pro",
                    "00000000-0000-0000-0000-000000000bbb",
                    ["motion_source"],
                    ["motion"],
                    ["pose_pos"],
                    "Active",
                    False,
                ),
                # Discovered but never registered: no session_id, no events.
                (
                    "00000000-0000-0000-0000-000000000ddd",
                    "ghost",
                    None,
                    [],
                    [],
                    [],
                    "Discovered",
                    False,
                ),
                # Closed session: already emitted session_left.
                (
                    "00000000-0000-0000-0000-000000000eee",
                    "gone",
                    "00000000-0000-0000-0000-000000000fff",
                    ["operator"],
                    ["mapping"],
                    [],
                    "Closed",
                    False,
                ),
            ]

    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _Native)
    server = server_module.MotionStageServer(name="unit")

    catalog = server.sessions()
    assert [entry["device_name"] for entry in catalog] == ["iPad Pro"]
    entry = catalog[0]
    assert entry["session_id"] == "00000000-0000-0000-0000-000000000bbb"
    assert entry["advertised_attributes"] == ["pose_pos"]
    assert entry["is_host"] is False


def test_host_session_events_never_reach_on_client_event(monkeypatch):
    """The host session's join/leave rides the event plane but must not surface
    as client-catalog deltas (the Blender addon would list a phantom 'host')."""

    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _FakeNative)
    server = server_module.MotionStageServer(name="unit")
    captured: list[dict[str, object]] = []
    server.emit_client_event = captured.append  # type: ignore[method-assign]

    host_session = _FakeNative().host_session_id()
    server._dispatch_state_event(  # type: ignore[attr-defined]
        {
            "seq": 1,
            "type": "session_joined",
            "session_id": host_session,
            "device_id": "00000000-0000-0000-0000-000000000h0d",
            "device_name": "host",
            "roles": ["scene_author", "operator"],
        }
    )
    server._dispatch_state_event(  # type: ignore[attr-defined]
        {
            "seq": 2,
            "type": "session_left",
            "session_id": host_session,
            "reason": "superseded by reconnect",
        }
    )
    assert captured == []

    # A real device session still flows through unchanged.
    server._dispatch_state_event(  # type: ignore[attr-defined]
        {
            "seq": 3,
            "type": "session_joined",
            "session_id": "00000000-0000-0000-0000-000000000bbb",
            "device_id": "00000000-0000-0000-0000-000000000ccc",
            "device_name": "iPad Pro",
            "roles": ["operator"],
        }
    )
    assert [event["kind"] for event in captured] == ["upsert"]


def test_session_left_reuses_device_identity_from_join(monkeypatch):
    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _FakeNative)
    server = server_module.MotionStageServer(name="unit")
    captured: list[dict[str, object]] = []
    server.emit_client_event = captured.append  # type: ignore[method-assign]

    server._dispatch_state_event(  # type: ignore[attr-defined]
        {
            "seq": 1,
            "type": "session_joined",
            "session_id": "00000000-0000-0000-0000-000000000bbb",
            "device_id": "00000000-0000-0000-0000-000000000ccc",
            "device_name": "iPad Pro",
            "roles": ["operator"],
        }
    )
    server._dispatch_state_event(  # type: ignore[attr-defined]
        {
            "seq": 2,
            "type": "session_left",
            "session_id": "00000000-0000-0000-0000-000000000bbb",
            "reason": "idle timeout",
        }
    )

    assert captured[0]["kind"] == "upsert"
    assert captured[-1]["kind"] == "removed"
    assert captured[-1]["device_id"] == "00000000-0000-0000-0000-000000000ccc"
    assert captured[-1]["device_name"] == "iPad Pro"
    assert captured[-1]["reason"] == "idle timeout"


def test_scene_events_and_lag_refresh_the_snapshot(monkeypatch):
    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _FakeNative)
    server = server_module.MotionStageServer(name="unit")
    snapshots: list[dict[str, object]] = []
    server.emit_scene_snapshot = snapshots.append  # type: ignore[method-assign]

    server._dispatch_state_event(  # type: ignore[attr-defined]
        {"seq": 1, "type": "scene_loaded", "scene_id": "s1", "name": "shot"}
    )
    server._dispatch_state_event(  # type: ignore[attr-defined]
        {"seq": 2, "type": "scene_activated", "scene_id": "s1"}
    )
    server._dispatch_state_event(  # type: ignore[attr-defined]
        {"type": "event_stream_lagged", "skipped": 7}
    )

    assert len(snapshots) == 3
    assert all(snapshot["mode"] == "idle" for snapshot in snapshots)


def test_mode_events_drive_attribute_poll_cadence(monkeypatch):
    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _FakeNative)
    server = server_module.MotionStageServer(name="unit")

    idle_interval = server._current_attribute_poll_interval_s()  # type: ignore[attr-defined]
    server._dispatch_state_event(  # type: ignore[attr-defined]
        {"seq": 1, "type": "mode_changed", "mode": "live"}
    )
    live_interval = server._current_attribute_poll_interval_s()  # type: ignore[attr-defined]
    server._dispatch_state_event(  # type: ignore[attr-defined]
        {"seq": 2, "type": "mode_changed", "mode": "idle"}
    )

    assert live_interval < idle_interval
    assert server._current_attribute_poll_interval_s() == idle_interval  # type: ignore[attr-defined]


def test_runtime_attribute_values_accepts_legacy_row_shape(monkeypatch):
    class _Native(_FakeNative):
        def runtime_attribute_values(self):
            return [("Camera", "position", [1.0, 2.0, 3.0])]

    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _Native)
    server = server_module.MotionStageServer(name="unit")
    rows = server.runtime_attribute_values()
    assert rows == [
        server_module.BakeAttributeValue(
            object_id="",
            object_name="Camera",
            attribute_name="position",
            value=[1.0, 2.0, 3.0],
        )
    ]


def test_take_and_bake_controls_are_exposed(monkeypatch):
    class _Native(_FakeNative):
        def list_takes(self, scene_id: str | None):
            assert scene_id in {None, "00000000-0000-0000-0000-000000000000"}
            return [
                (
                    "00000000-0000-0000-0000-000000000111",
                    "00000000-0000-0000-0000-000000000000",
                    "Take 001",
                    "/tmp/take-001.cmtrk",
                    100,
                    20,
                    True,
                    False,
                )
            ]

        def select_take(self, take_id: str) -> str:
            assert take_id == "00000000-0000-0000-0000-000000000111"
            return take_id

        def playback_play(self, take_id: str, looping: bool):
            assert take_id == "00000000-0000-0000-0000-000000000111"
            assert looping is True
            return ("playing", 0, True)

        def playback_pause(self, take_id: str):
            return ("paused", 10, True)

        def playback_seek(self, take_id: str, seek_ns: int, looping: bool):
            assert seek_ns == 999
            return ("paused", seek_ns, looping)

        def playback_stop(self, take_id: str):
            return ("stopped", 999, False)

        def delete_take(self, take_id: str) -> None:
            assert take_id == "00000000-0000-0000-0000-000000000111"

        def open_take_bake_cursor(self, take_id: str, sampling_mode: str):
            assert sampling_mode == "captured"
            return ("00000000-0000-0000-0000-000000000222", 20)

        def read_take_bake_frame(self, cursor_id: str):
            assert cursor_id == "00000000-0000-0000-0000-000000000222"
            return (
                0,
                100,
                [
                    (
                        "00000000-0000-0000-0000-000000000333",
                        "position",
                        [1.0, 2.0, 3.0],
                    )
                ],
            )

        def seek_take_bake_frame(self, cursor_id: str, frame_index: int):
            assert frame_index == 1
            return (
                1,
                200,
                [
                    (
                        "00000000-0000-0000-0000-000000000333",
                        "position",
                        [2.0, 3.0, 4.0],
                    )
                ],
            )

        def close_take_bake_cursor(self, cursor_id: str) -> None:
            assert cursor_id == "00000000-0000-0000-0000-000000000222"

    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _Native)
    server = server_module.MotionStageServer(name="unit")
    take_id = server_module.UUID("00000000-0000-0000-0000-000000000111")
    cursor_id = server_module.UUID("00000000-0000-0000-0000-000000000222")

    takes = server.list_takes()
    assert takes[0].take_id == str(take_id)
    assert server.select_take(take_id) == take_id
    assert server.playback_play(take_id, looping=True).state == "playing"
    assert server.playback_pause(take_id).state == "paused"
    assert server.playback_seek(take_id, 999, looping=False).playhead_ns == 999
    assert server.playback_stop(take_id).state == "stopped"
    server.delete_take(take_id)

    opened = server.open_take_bake_cursor(take_id)
    assert opened.cursor_id == str(cursor_id)
    assert server.read_take_bake_frame(cursor_id).frame_index == 0
    assert server.seek_take_bake_frame(cursor_id, 1).frame_index == 1
    server.close_take_bake_cursor(cursor_id)


def test_native_bridge_event_stream_end_to_end(monkeypatch):
    """Full-stack check against the compiled pyo3 bridge (skipped when the
    native extension is not built in this environment)."""
    native = pytest.importorskip("motionstage_sdk_rust")

    def _make_native(name: str | None = None):
        return native.MotionStageServer(name=name, discoverable=False)

    monkeypatch.setattr(server_module, "_NativeMotionStageServer", _make_native)
    server = server_module.MotionStageServer(name="pytest-native")
    delegate = _CapturingDelegate()
    server.bind_delegate(delegate)

    server.start()
    try:
        # The in-process host session must never surface as a client.
        assert server.sessions() == []
        scene_id = server.upsert_scene(
            {
                "name": "shot",
                "objects": [
                    {
                        "name": "camera",
                        "attributes": [
                            {
                                "name": "position",
                                "type": "vec3f",
                                "default_value": [0.0, 0.0, 0.0],
                            }
                        ],
                    }
                ],
            }
        )
        server.set_active_scene(scene_id)
        server.set_mode("live")

        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            if (
                delegate.by_kind["scene"]
                and delegate.by_kind["mode"]
                and delegate.by_kind["attr"]
            ):
                break
            time.sleep(0.02)
    finally:
        server.stop()

    # Initial snapshot fired, and the scene mutations re-emitted it.
    assert delegate.by_kind["scene"]
    latest_snapshot = delegate.by_kind["scene"][-1]
    assert any(scene["name"] == "shot" for scene in latest_snapshot["scenes"])

    # The mode change arrived via the event plane, not a poll.
    assert any(event.get("mode") == "live" for event in delegate.by_kind["mode"])

    # The data-plane attribute batch flowed from the same pump thread.
    batch = delegate.by_kind["attr"][-1]
    assert any(
        row["object"] == "camera" and row["attribute"] == "position" for row in batch
    )
