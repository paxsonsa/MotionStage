use std::sync::Arc;

use motionstage_media::{IceCandidate, SdpMessage, SdpType};
use thiserror::Error;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
    },
    ice_transport::ice_candidate::RTCIceCandidateInit,
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription, RTCPeerConnection,
    },
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::track_local_static_sample::TrackLocalStaticSample,
};

pub struct WebRtcSession {
    peer: Arc<RTCPeerConnection>,
}

impl WebRtcSession {
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

    pub async fn add_h264_track(&self, stream_id: &str, track_id: &str) -> Result<(), WebRtcError> {
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: "video/H264".into(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line: "".into(),
                rtcp_feedback: vec![],
            },
            track_id.into(),
            stream_id.into(),
        ));

        self.peer
            .add_track(track)
            .await
            .map_err(|err| WebRtcError::Track(err.to_string()))?;
        Ok(())
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
