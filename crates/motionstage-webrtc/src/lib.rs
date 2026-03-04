use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use motionstage_media::{IceCandidate, SdpMessage, SdpType, VideoCodec};
use thiserror::Error;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
    },
    ice_transport::ice_candidate::RTCIceCandidateInit,
    interceptor::registry::Registry,
    media::Sample,
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription, RTCPeerConnection,
    },
    rtp_transceiver::{rtp_codec::RTCRtpCodecCapability, RTCPFeedback},
    track::track_local::track_local_static_sample::TrackLocalStaticSample,
};

pub struct WebRtcSession {
    peer: Arc<RTCPeerConnection>,
    track: tokio::sync::Mutex<Option<Arc<TrackLocalStaticSample>>>,
}

impl WebRtcSession {
    fn codec_fmtp_line(codec: VideoCodec) -> &'static str {
        match codec {
            // Constrained Baseline Profile, Level 3.1 (42e01f) — matches openh264's default
            // output. Explicit profile-level-id is required for deterministic codec matching
            // with the media engine's registered H.264 variants and correct iOS decoder
            // configuration (CAVLC entropy coding).
            VideoCodec::H264 => "profile-level-id=42e01f;level-asymmetry-allowed=1;packetization-mode=1",
            _ => "",
        }
    }

    pub async fn new() -> Result<Self, WebRtcError> {
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .map_err(|err| WebRtcError::Peer(err.to_string()))?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .map_err(|err| WebRtcError::Peer(err.to_string()))?;
        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();
        let peer = api
            .new_peer_connection(RTCConfiguration::default())
            .await
            .map_err(|err| WebRtcError::Peer(err.to_string()))?;
        Ok(Self {
            peer: Arc::new(peer),
            track: tokio::sync::Mutex::new(None),
        })
    }

    pub fn peer_state(&self) -> RTCPeerConnectionState {
        self.peer.connection_state()
    }

    pub async fn create_offer(&self) -> Result<SdpMessage, WebRtcError> {
        let offer = self
            .peer
            .create_offer(None)
            .await
            .map_err(|err| WebRtcError::Sdp(err.to_string()))?;
        self.peer
            .set_local_description(offer.clone())
            .await
            .map_err(|err| WebRtcError::Sdp(err.to_string()))?;
        Ok(SdpMessage {
            ty: SdpType::Offer,
            sdp: offer.sdp,
        })
    }

    pub async fn create_answer(&self) -> Result<SdpMessage, WebRtcError> {
        let answer = self
            .peer
            .create_answer(None)
            .await
            .map_err(|err| WebRtcError::Sdp(err.to_string()))?;
        self.peer
            .set_local_description(answer.clone())
            .await
            .map_err(|err| WebRtcError::Sdp(err.to_string()))?;
        Ok(SdpMessage {
            ty: SdpType::Answer,
            sdp: answer.sdp,
        })
    }

    pub async fn apply_remote_sdp(&self, message: SdpMessage) -> Result<(), WebRtcError> {
        let description = match message.ty {
            SdpType::Offer => RTCSessionDescription::offer(message.sdp)
                .map_err(|err| WebRtcError::Sdp(err.to_string()))?,
            SdpType::Answer => RTCSessionDescription::answer(message.sdp)
                .map_err(|err| WebRtcError::Sdp(err.to_string()))?,
        };

        self.peer
            .set_remote_description(description)
            .await
            .map_err(|err| WebRtcError::Sdp(err.to_string()))
    }

    pub async fn add_ice_candidate(&self, candidate: IceCandidate) -> Result<(), WebRtcError> {
        self.peer
            .add_ice_candidate(RTCIceCandidateInit {
                candidate: candidate.candidate,
                sdp_mid: candidate.sdp_mid,
                sdp_mline_index: candidate.sdp_mline_index,
                username_fragment: None,
            })
            .await
            .map_err(|err| WebRtcError::Ice(err.to_string()))
    }

    pub async fn add_video_track(
        &self,
        codec: VideoCodec,
        stream_id: &str,
        track_id: &str,
    ) -> Result<Arc<TrackLocalStaticSample>, WebRtcError> {
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: codec.mime_type().into(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line: Self::codec_fmtp_line(codec).into(),
                // Must match the media engine's registered RTCP feedback so the SDP
                // negotiation enables PLI/NACK/FIR — without these, the receiver
                // cannot request keyframes on packet loss.
                rtcp_feedback: vec![
                    RTCPFeedback { typ: "goog-remb".to_owned(), parameter: "".to_owned() },
                    RTCPFeedback { typ: "ccm".to_owned(), parameter: "fir".to_owned() },
                    RTCPFeedback { typ: "nack".to_owned(), parameter: "".to_owned() },
                    RTCPFeedback { typ: "nack".to_owned(), parameter: "pli".to_owned() },
                ],
            },
            track_id.into(),
            stream_id.into(),
        ));

        self.peer
            .add_track(
                Arc::clone(&track) as Arc<dyn webrtc::track::track_local::TrackLocal + Send + Sync>
            )
            .await
            .map_err(|err| WebRtcError::Track(err.to_string()))?;

        *self.track.lock().await = Some(Arc::clone(&track));
        Ok(track)
    }

    pub async fn add_h264_track(
        &self,
        stream_id: &str,
        track_id: &str,
    ) -> Result<Arc<TrackLocalStaticSample>, WebRtcError> {
        self.add_video_track(VideoCodec::H264, stream_id, track_id)
            .await
    }

    pub async fn write_sample(&self, data: Bytes, duration: Duration) -> Result<(), WebRtcError> {
        let guard = self.track.lock().await;
        let track = guard
            .as_ref()
            .ok_or_else(|| WebRtcError::Track("no track added yet".into()))?;
        track
            .write_sample(&Sample {
                data,
                duration,
                ..Default::default()
            })
            .await
            .map_err(|err| WebRtcError::Track(err.to_string()))
    }

    pub async fn has_track(&self) -> bool {
        self.track.lock().await.is_some()
    }
}

#[derive(Debug, Error)]
pub enum WebRtcError {
    #[error("peer error: {0}")]
    Peer(String),
    #[error("sdp error: {0}")]
    Sdp(String),
    #[error("ice error: {0}")]
    Ice(String),
    #[error("track error: {0}")]
    Track(String),
}

#[cfg(test)]
mod tests {
    use super::WebRtcSession;
    use motionstage_media::SdpType;

    #[tokio::test]
    async fn create_offer_returns_offer_sdp() {
        let session = WebRtcSession::new().await.expect("session should build");
        let offer = session
            .create_offer()
            .await
            .expect("offer should be generated");
        assert_eq!(offer.ty, SdpType::Offer);
        assert!(!offer.sdp.is_empty());
    }
}
