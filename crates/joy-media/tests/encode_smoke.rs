//! Smoke tests proving the native encoder dependencies build and behave as
//! the media plane expects on this host.
//!
//! These pin the exact `openh264` and `opus` API surfaces the encoder tasks
//! rely on: I420 input produces an Annex-B H.264 bitstream whose first access
//! unit carries SPS/PPS/IDR (so a decoder can join from the start), a forced
//! intra frame re-emits an IDR mid-stream, and 20 ms of f32 PCM round-trips
//! through Opus at the sample count WebRTC expects.

use openh264::OpenH264API;
use openh264::encoder::{Encoder, EncoderConfig, FrameType};
use openh264::formats::YUVSource;

/// Minimal owned I420 buffer implementing the encoder's input trait.
struct TestI420 {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl TestI420 {
    /// Builds a flat gray frame with a per-frame brightness ramp so
    /// consecutive frames differ and the encoder has real work to do.
    fn new(width: usize, height: usize, seed: u8) -> Self {
        Self {
            width,
            height,
            y: vec![seed.wrapping_add(64); width * height],
            u: vec![128; width * height / 4],
            v: vec![128; width * height / 4],
        }
    }
}

impl YUVSource for TestI420 {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

/// Collects the NAL unit types present in an Annex-B bitstream.
fn nal_types(annexb: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < annexb.len() {
        let (start, len) = if annexb[i..].starts_with(&[0, 0, 0, 1]) {
            (i + 4, 4)
        } else if annexb[i..].starts_with(&[0, 0, 1]) {
            (i + 3, 3)
        } else {
            i += 1;
            continue;
        };
        if start < annexb.len() {
            types.push(annexb[start] & 0x1f);
        }
        i += len;
    }
    types
}

#[test]
fn h264_first_frame_carries_sps_pps_idr() {
    let config = EncoderConfig::new();
    let mut encoder =
        Encoder::with_api_config(OpenH264API::from_source(), config).expect("encoder construction");

    let frame = TestI420::new(320, 240, 0);
    let bitstream = encoder.encode(&frame).expect("encode");
    assert_eq!(bitstream.frame_type(), FrameType::IDR);

    let bytes = bitstream.to_vec();
    let types = nal_types(&bytes);
    assert!(types.contains(&7), "missing SPS in {types:?}");
    assert!(types.contains(&8), "missing PPS in {types:?}");
    assert!(types.contains(&5), "missing IDR slice in {types:?}");
}

#[test]
fn h264_force_intra_frame_yields_new_idr() {
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), EncoderConfig::new())
        .expect("encoder construction");

    let first = encoder.encode(&TestI420::new(320, 240, 0)).expect("encode");
    assert_eq!(first.frame_type(), FrameType::IDR);

    let second = encoder.encode(&TestI420::new(320, 240, 8)).expect("encode");
    assert_ne!(second.frame_type(), FrameType::IDR);

    encoder.force_intra_frame();
    let third = encoder
        .encode(&TestI420::new(320, 240, 16))
        .expect("encode");
    assert_eq!(third.frame_type(), FrameType::IDR);
    let types = nal_types(&third.to_vec());
    assert!(
        types.contains(&5),
        "forced intra produced no IDR slice: {types:?}"
    );
}

#[test]
fn h264_encoder_survives_resolution_change() {
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), EncoderConfig::new())
        .expect("encoder construction");

    let small = encoder
        .encode(&TestI420::new(320, 240, 0))
        .expect("encode small");
    assert_eq!(small.frame_type(), FrameType::IDR);

    // The crate re-initializes the encoder internally on dimension change and
    // the new stream must restart with an IDR + parameter sets.
    let large = encoder
        .encode(&TestI420::new(640, 480, 0))
        .expect("encode large");
    assert_eq!(large.frame_type(), FrameType::IDR);
    let types = nal_types(&large.to_vec());
    assert!(
        types.contains(&7) && types.contains(&8),
        "no SPS/PPS after reinit: {types:?}"
    );
}

#[test]
fn opus_roundtrips_20ms_of_f32_pcm() {
    let sample_rate = 48_000u32;
    let frames = (sample_rate / 50) as usize; // 20 ms

    let mut encoder =
        opus::Encoder::new(sample_rate, opus::Channels::Mono, opus::Application::Audio)
            .expect("opus encoder");

    // A quiet 440 Hz tone; silence can legitimately encode to near-nothing
    // with DTX-style optimizations, a tone cannot.
    let pcm: Vec<f32> = (0..frames)
        .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / sample_rate as f32).sin() * 0.25)
        .collect();

    let mut packet = vec![0u8; 1500];
    let len = encoder.encode_float(&pcm, &mut packet).expect("encode");
    assert!(len > 0, "empty opus packet");

    let mut decoder = opus::Decoder::new(sample_rate, opus::Channels::Mono).expect("decoder");
    let mut out = vec![0f32; frames];
    let decoded = decoder
        .decode_float(&packet[..len], &mut out, false)
        .expect("decode");
    assert_eq!(decoded, frames, "decoded frame count != 20 ms");
}
