"""CineMotion Python SDK (strict OOP delegate API)."""

from .server import (
    CineServer,
    CineSession,
    MappingManager,
    RecordingController,
    SceneRegistry,
    SecurityMode,
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
    "CineServer",
    "CineSession",
    "MappingManager",
    "RecordingController",
    "SceneRegistry",
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
