use bytes::Bytes;
use openh264::encoder::{Encoder, EncoderConfig, FrameRate, IntraFramePeriod};
use openh264::formats::{BgraSliceU8, RgbaSliceU8};
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;
use tracing::{debug, info};

use crate::MediaError;

/// Describe a NAL unit type number for diagnostics.
fn nal_type_name(t: u8) -> &'static str {
    match t {
        1 => "P-slice",
        5 => "IDR",
        6 => "SEI",
        7 => "SPS",
        8 => "PPS",
        _ => "other",
    }
}

/// Scan Annex B bitstream and return (nal_type, offset) pairs.
fn scan_nal_units(data: &[u8]) -> Vec<(u8, usize)> {
    let mut result = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        let (is_start, skip) = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            (true, 3)
        } else if i + 4 < data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            (true, 4)
        } else {
            (false, 1)
        };

        if is_start {
            let pos = i + skip;
            if pos < data.len() {
                result.push((data[pos] & 0x1F, pos));
            }
            i = pos + 1;
        } else {
            i += skip;
        }
    }
    result
}

/// H.264 encoder wrapping Cisco OpenH264.
///
/// Accepts raw RGBA pixel data and produces H.264 NAL unit bitstream
/// suitable for writing to a WebRTC `TrackLocalStaticSample`.
pub struct H264Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
    yuv_buf: YUVBuffer,
    emitted_first_frame: bool,
    frame_count: u64,
}

impl H264Encoder {
    /// Create a new encoder for the given resolution and target bitrate.
    pub fn new(width: u32, height: u32, fps: f32, bitrate_bps: u32) -> Result<Self, MediaError> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(MediaError::Encoder(
                "width and height must be positive and even".into(),
            ));
        }

        let keyframe_interval = fps.round().max(1.0) as u32;
        let config = EncoderConfig::new()
            .max_frame_rate(FrameRate::from_hz(fps))
            .intra_frame_period(IntraFramePeriod::from_num_frames(keyframe_interval))
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
            emitted_first_frame: false,
            frame_count: 0,
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

        if !self.emitted_first_frame {
            self.encoder.force_intra_frame();
            self.emitted_first_frame = true;
        }

        let bitstream = self
            .encoder
            .encode(&self.yuv_buf)
            .map_err(|err| MediaError::Encoder(err.to_string()))?;

        let encoded = Bytes::from(bitstream.to_vec());
        self.log_frame(&encoded);
        Ok(encoded)
    }

    /// Encode a raw BGRA frame into H.264 NAL units.
    ///
    /// `bgra` must contain exactly `width * height * 4` bytes.
    /// Returns the encoded H.264 bitstream (Annex B format).
    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Bytes, MediaError> {
        let w = self.width as usize;
        let h = self.height as usize;
        let expected = w * h * 4;

        if bgra.len() != expected {
            return Err(MediaError::Encoder(format!(
                "expected {expected} BGRA bytes, got {}",
                bgra.len()
            )));
        }

        let bgra_source = BgraSliceU8::new(bgra, (w, h));
        self.yuv_buf.read_rgb(bgra_source);

        if !self.emitted_first_frame {
            self.encoder.force_intra_frame();
            self.emitted_first_frame = true;
        }

        let bitstream = self
            .encoder
            .encode(&self.yuv_buf)
            .map_err(|err| MediaError::Encoder(err.to_string()))?;

        let encoded = Bytes::from(bitstream.to_vec());
        self.log_frame(&encoded);
        Ok(encoded)
    }

    /// Log encoded frame diagnostics. First frame logs NAL unit types at info level.
    fn log_frame(&mut self, encoded: &Bytes) {
        self.frame_count += 1;

        debug!(
            frame = self.frame_count,
            bytes = encoded.len(),
            width = self.width,
            height = self.height,
            "h264: encoded frame"
        );

        // On the first frame only, log NAL unit details at info level.
        if self.frame_count == 1 {
            let nals = scan_nal_units(encoded);
            if !nals.is_empty() {
                let descriptions: Vec<String> = nals
                    .iter()
                    .map(|(t, _)| format!("{}({})", nal_type_name(*t), t))
                    .collect();
                info!(
                    nal_units = %descriptions.join(", "),
                    bytes = encoded.len(),
                    "h264: first frame NAL units"
                );
            }
        }
    }

    /// Force the next encoded frame to be an IDR keyframe.
    pub fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
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

    /// Finds Annex B NAL unit types in an H.264 bitstream.
    /// Returns a sorted, deduplicated list of NAL unit types found.
    fn find_nal_types(data: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut i = 0;
        while i + 3 < data.len() {
            // Look for 3-byte (00 00 01) or 4-byte (00 00 00 01) start codes
            let (is_start, skip) = if i + 3 < data.len()
                && data[i] == 0
                && data[i + 1] == 0
                && data[i + 2] == 1
            {
                (true, 3)
            } else if i + 4 < data.len()
                && data[i] == 0
                && data[i + 1] == 0
                && data[i + 2] == 0
                && data[i + 3] == 1
            {
                (true, 4)
            } else {
                (false, 1)
            };

            if is_start {
                let nal_header_pos = i + skip;
                if nal_header_pos < data.len() {
                    let nal_type = data[nal_header_pos] & 0x1F;
                    types.push(nal_type);
                }
                i = nal_header_pos + 1;
            } else {
                i += skip;
            }
        }
        types.sort();
        types.dedup();
        types
    }

    #[test]
    fn encode_1280x720_produces_valid_h264() {
        let width = 1280u32;
        let height = 720u32;
        let mut enc = H264Encoder::new(width, height, 24.0, 2_000_000).unwrap();
        let pixels = (width * height) as usize;
        // Solid blue frame
        let rgba: Vec<u8> = (0..pixels).flat_map(|_| [0u8, 0, 255, 255]).collect();

        let encoded = enc.encode_rgba(&rgba).unwrap();

        // Must produce meaningful output
        assert!(
            encoded.len() > 100,
            "IDR frame should be >100 bytes, got {}",
            encoded.len()
        );

        let nal_types = find_nal_types(&encoded);

        // SPS (type 7) must be present
        assert!(
            nal_types.contains(&7),
            "missing SPS NAL unit; found types: {nal_types:?}"
        );
        // PPS (type 8) must be present
        assert!(
            nal_types.contains(&8),
            "missing PPS NAL unit; found types: {nal_types:?}"
        );
        // IDR slice (type 5) must be present (first frame is forced IDR)
        assert!(
            nal_types.contains(&5),
            "missing IDR NAL unit; found types: {nal_types:?}"
        );
    }
}
