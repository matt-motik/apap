//! DoP (DSD over PCM) framing, ТЗ 5.1 §8.2 (этап 6.6/6.8).
//!
//! Each interleaved raw-DSD byte (8 one-bit samples per channel) is packed
//! into a 16-bit word: the most significant byte holds the DSD data and the
//! least significant byte carries the 0x05/0xFA marker that toggles on every
//! PCM frame (DoP spec). Both channels of one frame share the same marker
//! phase. The container rate equals the DSD rate / 8, i.e. 352.8 kHz for
//! DSD64 — a bit-exact transport of the full 2.8224 MHz stream.

/// Marker byte for even frames.
pub const DOP_MARKER_EVEN: u8 = 0x05;
/// Marker byte for odd frames.
pub const DOP_MARKER_ODD: u8 = 0xFA;

/// Zero-alloc DoP framer for the real-time audio path.
///
/// `frame()` reads interleaved DSD bytes (`ch` bytes per frame) and writes
/// the packed 16-bit words (1 word per byte) into `out`. The marker phase is
/// kept across calls so a long stream stays in sync.
#[derive(Debug, Clone, Copy, Default)]
pub struct DoPFramer {
    /// Whether the next emitted frame carries the odd (0xFA) marker.
    marker_phase: bool,
}

impl DoPFramer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Frame `raw` (interleaved DSD bytes, whole frames of `ch` channels) into
    /// `out`. `out` must hold at least `raw.len()` words. Writes exactly
    /// `raw.len()` words.
    pub fn frame(&mut self, raw: &[u8], ch: usize, out: &mut [u16]) {
        debug_assert!(ch > 0);
        let words = raw.len() / ch;
        debug_assert!(out.len() >= words * ch);
        for (f, chunk) in out[..words * ch].chunks_mut(ch).enumerate() {
            let marker = if self.marker_phase {
                DOP_MARKER_ODD
            } else {
                DOP_MARKER_EVEN
            };
            for (dst, &byte) in chunk.iter_mut().zip(raw[f * ch..f * ch + ch].iter()) {
                *dst = ((byte as u16) << 8) | marker as u16;
            }
            self.marker_phase = !self.marker_phase;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dop_marker_pattern() {
        let mut f = DoPFramer::new();
        // 3 stereo frames: [L0,R0][L1,R1][L2,R2].
        let raw = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let mut out = [0u16; 6];
        f.frame(&raw, 2, &mut out);
        // Both channels share the same marker; DSD byte goes to the MSB.
        assert_eq!(out[0], (0x01u16 << 8) | DOP_MARKER_EVEN as u16);
        assert_eq!(out[1], (0x02u16 << 8) | DOP_MARKER_EVEN as u16);
        assert_eq!(out[2], (0x03u16 << 8) | DOP_MARKER_ODD as u16);
        assert_eq!(out[3], (0x04u16 << 8) | DOP_MARKER_ODD as u16);
        assert_eq!(out[4], (0x05u16 << 8) | DOP_MARKER_EVEN as u16);
        assert_eq!(out[5], (0x06u16 << 8) | DOP_MARKER_EVEN as u16);
        assert!(f.marker_phase); // 3 frames toggled from even -> odd
    }

    #[test]
    fn dop_phase_continues_across_calls() {
        let mut f = DoPFramer::new();
        let mut out = [0u16; 4];
        // 2 frames -> marker returned to even.
        f.frame(&[0x11, 0x22, 0x33, 0x44], 2, &mut out);
        assert!(!f.marker_phase);
        // Next call starts on an even frame again (phase persists).
        let mut out2 = [0u16; 2];
        f.frame(&[0x55, 0x66], 2, &mut out2);
        assert_eq!(out2[0], (0x55u16 << 8) | DOP_MARKER_EVEN as u16);
        assert_eq!(out2[1], (0x66u16 << 8) | DOP_MARKER_EVEN as u16);
    }
}