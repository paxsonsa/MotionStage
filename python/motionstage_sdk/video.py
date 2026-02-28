from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from enum import Enum
from typing import Callable, Optional


class DynamicRange(str, Enum):
    SDR = "SDR"
    HDR10 = "HDR10"


class ColorPrimaries(str, Enum):
    BT709 = "BT.709"
    BT2020 = "BT.2020"


class TransferFunction(str, Enum):
    SRGB = "SRGB"
    PQ = "PQ"


@dataclass(frozen=True)
class VideoStreamDescriptor:
    width: int
    height: int
    fps: int
    dynamic_range: DynamicRange
    color_primaries: ColorPrimaries
    transfer: TransferFunction
    bit_depth: int

    def validate(self) -> None:
        if self.width <= 0 or self.height <= 0 or self.fps <= 0:
            raise ValueError("width, height, and fps must be > 0")

        if self.dynamic_range == DynamicRange.HDR10:
            if self.color_primaries != ColorPrimaries.BT2020:
                raise ValueError("HDR10 requires BT.2020 primaries")
            if self.transfer != TransferFunction.PQ:
                raise ValueError("HDR10 requires PQ transfer")
            if self.bit_depth != 10:
                raise ValueError("HDR10 requires 10-bit")


@dataclass(frozen=True)
class VideoFrame:
    timestamp_ns: int
    descriptor: VideoStreamDescriptor
    payload: bytes


class MainThreadDispatcher(ABC):
    @abstractmethod
    def dispatch(self, fn: Callable[[], None]) -> None:
        raise NotImplementedError


class InlineDispatcher(MainThreadDispatcher):
    def dispatch(self, fn: Callable[[], None]) -> None:
        fn()


class FramePullDelegate(ABC):
    @abstractmethod
    def get_frame(self, timestamp_ns: int) -> Optional[VideoFrame]:
        raise NotImplementedError


class FramePushSink(ABC):
    @abstractmethod
    def on_frame(self, frame: VideoFrame) -> None:
        raise NotImplementedError

    @abstractmethod
    def on_stream_state(self, state: str) -> None:
        raise NotImplementedError


class VideoStreamEndpoint:
    def __init__(
        self,
        descriptor: VideoStreamDescriptor,
        dispatcher: MainThreadDispatcher,
        pull_delegate: Optional[FramePullDelegate] = None,
        push_sink: Optional[FramePushSink] = None,
    ) -> None:
        descriptor.validate()
        if (pull_delegate is None) == (push_sink is None):
            raise ValueError("Provide exactly one of pull_delegate or push_sink")
        self._descriptor = descriptor
        self._dispatcher = dispatcher
        self._pull_delegate = pull_delegate
        self._push_sink = push_sink

    @property
    def descriptor(self) -> VideoStreamDescriptor:
        return self._descriptor

    def request_frame(self, timestamp_ns: int) -> Optional[VideoFrame]:
        if self._pull_delegate is None:
            return None
        return self._pull_delegate.get_frame(timestamp_ns)

    def push_frame(self, frame: VideoFrame) -> None:
        if self._push_sink is None:
            return
        self._dispatcher.dispatch(lambda: self._push_sink.on_frame(frame))

    def set_state(self, state: str) -> None:
        if self._push_sink is None:
            return
        self._dispatcher.dispatch(lambda: self._push_sink.on_stream_state(state))
