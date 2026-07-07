from __future__ import annotations

from abc import ABC, abstractmethod
from typing import Any


class SceneUpdateDelegate(ABC):
    """Callbacks fired from the SDK's background state-event pump.

    Every event dict delivered to the ``on_*_event`` callbacks below carries the
    control-plane envelope fields copied straight from the core event:

    * ``seq`` (int) — the server-assigned sequence number of the event. It is
      strictly monotonically increasing across the whole event stream, so a
      consumer can use it to discard stale snapshots / out-of-order deliveries
      and to guard registrations against reordering.
    * ``origin_session`` (str | None) — the session that caused the event, or
      None for server-originated events.
    * ``timestamp_ns`` (int) — server-monotonic emit time in nanoseconds.

    These ride alongside the event's ``type`` discriminator, the SDK-added
    ``kind`` label, and the type-specific payload. ``on_scene_snapshot`` receives
    a full snapshot whose top-level ``seq`` is the sequence the snapshot is
    consistent with (events with ``seq`` <= it are already folded in);
    ``on_attribute_batch`` is the data plane and carries no ``seq``.
    """

    @abstractmethod
    def on_scene_snapshot(self, snapshot: dict[str, Any]) -> None:
        raise NotImplementedError

    @abstractmethod
    def on_attribute_batch(self, batch: list[dict[str, Any]]) -> None:
        raise NotImplementedError

    @abstractmethod
    def on_mapping_event(self, event: dict[str, Any]) -> None:
        """A mapping lifecycle event. Carries the monotonic ``seq`` (see class
        docstring), a ``kind`` of created/updated/removed/lock_changed/released,
        and the mapping payload. Use ``seq`` to reject stale snapshots that would
        otherwise reorder registrations."""
        raise NotImplementedError

    @abstractmethod
    def on_mode_event(self, event: dict[str, Any]) -> None:
        raise NotImplementedError

    @abstractmethod
    def on_client_event(self, event: dict[str, Any]) -> None:
        raise NotImplementedError

    @abstractmethod
    def on_recording_event(self, event: dict[str, Any]) -> None:
        raise NotImplementedError
