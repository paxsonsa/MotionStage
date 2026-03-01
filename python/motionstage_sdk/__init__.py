"""MotionStage Python SDK (strict OOP delegate API)."""

from .server import (
    MotionStageServer,
    MotionStageSession,
    MappingManager,
    RecordingController,
    SecurityMode,
    TakeController,
)
from .video import (
    ColorPrimaries,
    DynamicRange,
    FramePushSink,
    FramePullDelegate,
    MainThreadDispatcher,
    TransferFunction,
    VideoFrame,
    VideoStreamDescriptor,
    VideoStreamEndpoint,
)
from .delegates import SceneUpdateDelegate

__all__ = [
    "MotionStageServer",
    "MotionStageSession",
    "MappingManager",
    "RecordingController",
    "TakeController",
    "SecurityMode",
    "SceneUpdateDelegate",
    "MainThreadDispatcher",
    "FramePullDelegate",
    "FramePushSink",
    "VideoFrame",
    "VideoStreamDescriptor",
    "VideoStreamEndpoint",
    "DynamicRange",
    "ColorPrimaries",
    "TransferFunction",
]
