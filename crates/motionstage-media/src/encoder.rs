use bytes::Bytes;
use openh264::encoder::{Encoder, EncoderConfig, FrameRate};
use openh264::formats::RgbaSliceU8;
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;

use crate::MediaError;

/// H.264 encoder wrapping Cisco OpenH264.
///
/// Accepts raw RGBA pixel data and produces H.264 NAL unit bitstream
/// suitable for writing to a WebRTC `TrackLocalStaticSample`.
pub struct H264Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
    yuv_buf: YUVBuffer,
}

impl H264Encoder {
    /// Create a new encoder for the given resolution and target bitrate.
    pub fn new(width: u32, height: u32, fps: f32, bitrate_bps: u32) -> Result<Self, MediaError> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(MediaError::Encoder(
                "width and height must be positive and even".into(),
            ));
        }

        let config = EncoderConfig::new()
            .max_frame_rate(FrameRate::from_hz(fps))
            .bitrate(openh264::encoder::BitRate::from_bps(bitrate_bps));

        let api = OpenH264API::from_source();
        let encoder = Encoder::with_api_config(api, config)
            .map_err(|err| MediaError::Encoder(err.to_string()))?;

        let yuv_buf = YUVBuffer::new(width as usize, height as usize);

        Ok(Self {
            encoder,
            width,
            height,
            yuv_buf,
        })
    }

    /// Encode a raw RGBA frame into H.264 NAL units.
    ///
    /// `rgba` must contain exactly `width * height * 4` bytes.
    /// Returns the encoded H.264 bitstream (Annex B format).
    pub fn encode_rgba(&mut self, rgba: &[u8]) -> Result<Bytes, MediaError> {
        let w = self.width as usize;
        let h = self.height as usize;
        let expected = w * h * 4;

        if rgba.len() != expected {
            return Err(MediaError::Encoder(format!(
                "expected {expected} RGBA bytes, got {}",
                rgba.len()
            )));
        }

        let rgba_source = RgbaSliceU8::new(rgba, (w, h));
        self.yuv_buf.read_rgb(rgba_source);

        let bitstream = self
            .encoder
            .encode(&self.yuv_buf)
            .map_err(|err| MediaError::Encoder(err.to_string()))?;

        Ok(Bytes::from(bitstream.to_vec()))
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_rejects_odd_dimensions() {
        let result = H264Encoder::new(1281, 720, 24.0, 2_000_000);
        assert!(result.is_err());
    }

    #[test]
    fn encoder_rejects_wrong_buffer_size() {
        let mut enc = H264Encoder::new(16, 16, 24.0, 500_000).unwrap();
        let result = enc.encode_rgba(&[0u8; 10]);
        assert!(result.is_err());
    }

    #[test]
    fn encode_solid_red_frame() {
        let mut enc = H264Encoder::new(16, 16, 24.0, 500_000).unwrap();
        let pixels = 16 * 16;
        let rgba: Vec<u8> = (0..pixels).flat_map(|_| [255u8, 0, 0, 255]).collect();
        let encoded = enc.encode_rgba(&rgba).unwrap();
        // Should produce some H.264 output (at minimum SPS/PPS + IDR)
        assert!(!encoded.is_empty());
    }
}
