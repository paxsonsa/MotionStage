"""MotionStage Python SDK (strict OOP delegate API)."""

from .server import (
    BakeAttributeValue,
    BakeCursorInfo,
    BakeFrame,
    MappingManager,
    MotionStageServer,
    MotionStageSession,
    PlaybackState,
    RecordingController,
    SecurityMode,
    ServerMetrics,
    TakeController,
    TakeInfo,
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
    "BakeAttributeValue",
    "BakeCursorInfo",
    "BakeFrame",
    "MappingManager",
    "MotionStageServer",
    "MotionStageSession",
    "PlaybackState",
    "RecordingController",
    "SceneUpdateDelegate",
    "SecurityMode",
    "ServerMetrics",
    "TakeController",
    "TakeInfo",
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
