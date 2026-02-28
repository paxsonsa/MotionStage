from __future__ import annotations

from abc import ABC, abstractmethod
from typing import Any


class SceneUpdateDelegate(ABC):
    @abstractmethod
    def on_scene_snapshot(self, snapshot: dict[str, Any]) -> None:
        raise NotImplementedError

    @abstractmethod
    def on_attribute_batch(self, batch: list[dict[str, Any]]) -> None:
        raise NotImplementedError

    @abstractmethod
    def on_mapping_event(self, event: dict[str, Any]) -> None:
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
