use bytes::Bytes;
pub use motionstage_protocol::{IceCandidate, SdpMessage, SdpType, SignalMessage, SignalPayload};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DynamicRange {
    Sdr,
    Hdr10,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorPrimaries {
    Bt709,
    Bt2020,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferFunction {
    Srgb,
    Pq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToneMapMode {
    None,
    Hdr10ToSdr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoStreamDescriptor {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub dynamic_range: DynamicRange,
    pub color_primaries: ColorPrimaries,
    pub transfer: TransferFunction,
    pub bit_depth: u8,
}

impl VideoStreamDescriptor {
    pub fn validate(&self) -> Result<(), MediaError> {
        if self.width == 0 || self.height == 0 || self.fps == 0 {
            return Err(MediaError::InvalidDescriptor(
                "width, height and fps must be non-zero".into(),
            ));
        }

        if self.dynamic_range == DynamicRange::Hdr10 {
            if self.color_primaries != ColorPrimaries::Bt2020 {
                return Err(MediaError::InvalidDescriptor(
                    "HDR10 requires BT.2020 primaries".into(),
                ));
            }
            if self.transfer != TransferFunction::Pq {
                return Err(MediaError::InvalidDescriptor(
                    "HDR10 requires PQ transfer".into(),
                ));
            }
            if self.bit_depth != 10 {
                return Err(MediaError::InvalidDescriptor(
                    "HDR10 requires 10-bit pipeline".into(),
                ));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoClientCapability {
    pub supports_hdr10: bool,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NegotiatedVideoStream {
    pub descriptor: VideoStreamDescriptor,
    pub tone_map: ToneMapMode,
}

pub fn negotiate_stream(
    master: &VideoStreamDescriptor,
    client: VideoClientCapability,
) -> Result<NegotiatedVideoStream, MediaError> {
    master.validate()?;

    if master.width > client.max_width
        || master.height > client.max_height
        || master.fps > client.max_fps
    {
        return Err(MediaError::UnsupportedDescriptor(
            "client capability cannot satisfy DCC-authored descriptor".into(),
        ));
    }

    if master.dynamic_range == DynamicRange::Hdr10 && !client.supports_hdr10 {
        return Ok(NegotiatedVideoStream {
            descriptor: VideoStreamDescriptor {
                width: master.width,
                height: master.height,
                fps: master.fps,
                dynamic_range: DynamicRange::Sdr,
                color_primaries: ColorPrimaries::Bt709,
                transfer: TransferFunction::Srgb,
                bit_depth: 8,
            },
            tone_map: ToneMapMode::Hdr10ToSdr,
        });
    }

    Ok(NegotiatedVideoStream {
        descriptor: master.clone(),
        tone_map: ToneMapMode::None,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoFrame {
    pub timestamp_ns: u64,
    pub descriptor: VideoStreamDescriptor,
    pub payload: Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Started,
    Stopped,
}

#[derive(Debug, Default, Clone)]
pub struct SignalingHub {
    queues: BTreeMap<Uuid, Vec<SignalMessage>>,
}

impl SignalingHub {
    pub fn enqueue(&mut self, message: SignalMessage) {
        self.queues
            .entry(message.to_device)
            .or_default()
            .push(message);
    }

    pub fn drain_for(&mut self, device_id: Uuid) -> Vec<SignalMessage> {
        self.queues.remove(&device_id).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRequest {
    pub timestamp_ns: u64,
}

pub trait MainThreadExecutor: Send + Sync + 'static {
    fn dispatch(&self, job: Box<dyn FnOnce() + Send + 'static>);
}

pub struct InlineMainThreadExecutor;

impl MainThreadExecutor for InlineMainThreadExecutor {
    fn dispatch(&self, job: Box<dyn FnOnce() + Send + 'static>) {
        job();
    }
}

pub trait FramePullDelegate: Send + Sync + 'static {
    fn get_frame(&self, request: FrameRequest) -> Option<VideoFrame>;
}

pub trait FramePushSink: Send + Sync + 'static {
    fn on_frame(&self, frame: VideoFrame);
    fn on_stream_state(&self, state: StreamState);
}

#[derive(Clone)]
pub enum VideoStreamMode {
    Pull(Arc<dyn FramePullDelegate>),
    Push(Arc<dyn FramePushSink>),
}

#[derive(Clone)]
pub struct VideoStreamEndpoint {
    descriptor: VideoStreamDescriptor,
    mode: VideoStreamMode,
    executor: Arc<dyn MainThreadExecutor>,
}

impl VideoStreamEndpoint {
    pub fn from_pull(
        descriptor: VideoStreamDescriptor,
        delegate: Arc<dyn FramePullDelegate>,
        executor: Arc<dyn MainThreadExecutor>,
    ) -> Result<Self, MediaError> {
        descriptor.validate()?;
        Ok(Self {
            descriptor,
            mode: VideoStreamMode::Pull(delegate),
            executor,
        })
    }

    pub fn from_push(
        descriptor: VideoStreamDescriptor,
        sink: Arc<dyn FramePushSink>,
        executor: Arc<dyn MainThreadExecutor>,
    ) -> Result<Self, MediaError> {
        descriptor.validate()?;
        Ok(Self {
            descriptor,
            mode: VideoStreamMode::Push(sink),
            executor,
        })
    }

    pub fn descriptor(&self) -> &VideoStreamDescriptor {
        &self.descriptor
    }

    pub fn request_frame(&self, request: FrameRequest) -> Option<VideoFrame> {
        match &self.mode {
            VideoStreamMode::Pull(delegate) => delegate.get_frame(request),
            VideoStreamMode::Push(_) => None,
        }
    }

    pub fn push_frame(&self, frame: VideoFrame) {
        if let VideoStreamMode::Push(sink) = &self.mode {
            let sink = Arc::clone(sink);
            self.executor
                .dispatch(Box::new(move || sink.on_frame(frame)));
        }
    }

    pub fn update_state(&self, state: StreamState) {
        if let VideoStreamMode::Push(sink) = &self.mode {
            let sink = Arc::clone(sink);
            self.executor
                .dispatch(Box::new(move || sink.on_stream_state(state)));
        }
    }
}

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("invalid video descriptor: {0}")]
    InvalidDescriptor(String),
    #[error("unsupported video descriptor: {0}")]
    UnsupportedDescriptor(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[test]
    fn hdr10_requires_correct_metadata() {
        let descriptor = VideoStreamDescriptor {
            width: 1920,
            height: 1080,
            fps: 24,
            dynamic_range: DynamicRange::Hdr10,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Pq,
            bit_depth: 10,
        };

        let err = descriptor.validate().unwrap_err();
        assert!(format!("{err}").contains("BT.2020"));
    }

    #[test]
    fn sdr_fallback_preserves_dcc_resolution() {
        let descriptor = VideoStreamDescriptor {
            width: 3840,
            height: 2160,
            fps: 60,
            dynamic_range: DynamicRange::Hdr10,
            color_primaries: ColorPrimaries::Bt2020,
            transfer: TransferFunction::Pq,
            bit_depth: 10,
        };
        let client = VideoClientCapability {
            supports_hdr10: false,
            max_width: 3840,
            max_height: 2160,
            max_fps: 60,
        };

        let negotiated = negotiate_stream(&descriptor, client).unwrap();
        assert_eq!(negotiated.descriptor.width, 3840);
        assert_eq!(negotiated.descriptor.height, 2160);
        assert_eq!(negotiated.descriptor.fps, 60);
        assert_eq!(negotiated.tone_map, ToneMapMode::Hdr10ToSdr);
    }

    #[test]
    fn descriptor_is_rejected_when_client_cannot_meet_dcc_resolution() {
        let descriptor = VideoStreamDescriptor {
            width: 3840,
            height: 2160,
            fps: 60,
            dynamic_range: DynamicRange::Sdr,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Srgb,
            bit_depth: 8,
        };
        let client = VideoClientCapability {
            supports_hdr10: true,
            max_width: 1920,
            max_height: 1080,
            max_fps: 60,
        };

        let err = negotiate_stream(&descriptor, client).unwrap_err();
        assert!(format!("{err}").contains("DCC-authored descriptor"));
    }

    #[test]
    fn signaling_hub_routes_and_drains_messages() {
        let mut hub = SignalingHub::default();
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();

        hub.enqueue(SignalMessage {
            from_device: a,
            to_device: b,
            payload: SignalPayload::Sdp(SdpMessage {
                ty: SdpType::Offer,
                sdp: "v=0".into(),
            }),
        });

        let msgs = hub.drain_for(b);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from_device, a);
        assert!(hub.drain_for(b).is_empty());
    }

    struct TestPushSink {
        frames: Mutex<Vec<VideoFrame>>,
        states: Mutex<Vec<StreamState>>,
    }

    impl TestPushSink {
        fn new() -> Self {
            Self {
                frames: Mutex::new(vec![]),
                states: Mutex::new(vec![]),
            }
        }
    }

    impl FramePushSink for TestPushSink {
        fn on_frame(&self, frame: VideoFrame) {
            self.frames.lock().push(frame);
        }

        fn on_stream_state(&self, state: StreamState) {
            self.states.lock().push(state);
        }
    }

    #[test]
    fn push_endpoint_delivers_frames_via_executor() {
        let descriptor = VideoStreamDescriptor {
            width: 1280,
            height: 720,
            fps: 30,
            dynamic_range: DynamicRange::Sdr,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Srgb,
            bit_depth: 8,
        };
        let sink = Arc::new(TestPushSink::new());
        let endpoint = VideoStreamEndpoint::from_push(
            descriptor.clone(),
            sink.clone(),
            Arc::new(InlineMainThreadExecutor),
        )
        .unwrap();

        endpoint.update_state(StreamState::Started);
        endpoint.push_frame(VideoFrame {
            timestamp_ns: 1,
            descriptor,
            payload: Bytes::from_static(&[1, 2, 3]),
        });

        assert_eq!(sink.states.lock().as_slice(), &[StreamState::Started]);
        assert_eq!(sink.frames.lock().len(), 1);
    }
}
