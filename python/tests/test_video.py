from cinemotion_sdk.video import (
    ColorPrimaries,
    DynamicRange,
    FramePushSink,
    InlineDispatcher,
    TransferFunction,
    VideoFrame,
    VideoStreamDescriptor,
    VideoStreamEndpoint,
)


def test_hdr10_validation():
    descriptor = VideoStreamDescriptor(
        width=1920,
        height=1080,
        fps=24,
        dynamic_range=DynamicRange.HDR10,
        color_primaries=ColorPrimaries.BT2020,
        transfer=TransferFunction.PQ,
        bit_depth=10,
    )
    descriptor.validate()


class _Sink(FramePushSink):
    def __init__(self) -> None:
        self.frames: list[VideoFrame] = []
        self.states: list[str] = []

    def on_frame(self, frame: VideoFrame) -> None:
        self.frames.append(frame)

    def on_stream_state(self, state: str) -> None:
        self.states.append(state)


def test_push_endpoint_dispatches_frame_and_state():
    descriptor = VideoStreamDescriptor(
        width=1920,
        height=1080,
        fps=24,
        dynamic_range=DynamicRange.SDR,
        color_primaries=ColorPrimaries.BT709,
        transfer=TransferFunction.SRGB,
        bit_depth=8,
    )
    sink = _Sink()
    endpoint = VideoStreamEndpoint(
        descriptor=descriptor,
        dispatcher=InlineDispatcher(),
        push_sink=sink,
    )

    frame = VideoFrame(timestamp_ns=1, descriptor=descriptor, payload=b"\x00")
    endpoint.push_frame(frame)
    endpoint.set_state("Started")

    assert len(sink.frames) == 1
    assert sink.states == ["Started"]
