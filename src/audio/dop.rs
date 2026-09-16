//! DoP (DSD over PCM) framing, ТЗ 5.1 §8.2 (этап 6.6/6.8).
//!
//! Frames the raw DSD byte stream in the container format agreed with the
//! reference implementations MPD (`src/pcm/Dop.cxx`, `DsdToDop`) and mpv
//! (`ad_dsd.c`): one PCM word carries **2 DSD bytes per channel** (16 DSD bit
//! samples), so the container rate is the DSD byte rate / 2 — 176.4 kHz for
//! DSD64 — while the full 2.8224 MHz stream stays bit-exact.
//!
//! Word layout (24-bit value, marker in bits 23-16, data 15-0):
//!   `(marker << 16) | (older_byte << 8) | newer_byte`
//! The marker alternates 0x05/0xFA on every output frame; both channels of a
//! frame share the same marker phase. The 24-bit value fits f32's 24-bit
//! mantissa exactly, so it survives the identity resampler; the audio-callback
//! left-shifts it by 8 to place the marker into bits 31-24 of the 32-bit
//! container (identical to mpv's `marker << 24 | d0 << 16 | d1 << 8` after the
//! S32 shift8 pass).

/// Marker byte for even frames.
pub const DOP_MARKER_EVEN: u8 = 0x05;
/// Marker byte for odd frames.
pub const DOP_MARKER_ODD: u8 = 0xFA;

/// Zero-alloc DoP framer for the real-time audio path.
///
/// `frame()` reads interleaved DSD bytes (`ch` bytes per frame) and writes one
/// 24-bit word per channel per pair of source frames into `out`. If a call
/// ends on an uneven frame boundary, the trailing half-frame bytes are kept
/// inside the framer and paired with the next call's first bytes, so a stream
/// split at any byte boundary stays bit-exact (no allocation: the carry buffer
/// is preallocated once at open time).
#[derive(Debug, Clone, Default)]
pub struct DoPFramer {
    /// Whether the next emitted frame carries the odd (0xFA) marker.
    marker_phase: bool,
    /// Trailing half-frame bytes (interleaved, `0 <= len < 2*ch`) carried
    /// over from the previous call.
    leftover: Vec<u8>,
}

impl DoPFramer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Preallocate the carry buffer that can hold one half-frame of `ch`
    /// channels. Called once at open time so the hot path never allocates.
    pub fn with_channels(&mut self, ch: usize) {
        self.leftover = Vec::with_capacity(ch);
    }

    /// Frame `raw` (interleaved DSD bytes, whole frames of `ch` channels) into
    /// `out` (interleaved 24-bit DoP words) and return the number of words
    /// written (`ch` words per consumed pair of source frames).
    ///
    /// After a previous call ended with a carried half-frame, `raw` is prepended
    /// to it so the pair grouping is preserved across fragment boundaries.
    pub fn frame(&mut self, raw: &[u8], ch: usize, out: &mut [u32]) -> usize {
        debug_assert!(ch > 0);
        let carry = self.leftover.len();
        let total = carry + raw.len();
        let pairs = total / (2 * ch); // source-frame pairs fully available
        let words = pairs * ch;
        debug_assert!(out.len() >= words);

        let mut src = 0usize; // absolute index into [leftover | raw]
        let mut dst = 0usize;
        while src < pairs * 2 * ch {
            let marker = if self.marker_phase {
                DOP_MARKER_ODD
            } else {
                DOP_MARKER_EVEN
            };
            for c in 0..ch {
                let older = self.byte_at(src + c, carry, raw);
                let newer = self.byte_at(src + ch + c, carry, raw);
                out[dst] = ((marker as u32) << 16) | ((older as u32) << 8) | newer as u32;
                dst += 1;
            }
            self.marker_phase = !self.marker_phase;
            src += 2 * ch;
        }

        // Keep bytes that cannot form a full pair as carry for the next call.
        self.leftover.clear();
        while src < total {
            self.leftover.push(self.byte_at(src, carry, raw));
            src += 1;
        }
        words
    }

    /// Byte at stream index `i` counting from the start of the concatenation
    /// `[leftover, raw]` (the leftover buffer is never empty here because the
    /// callers only use full frames, but the guard keeps the indexing sound).
    #[inline]
    fn byte_at(&self, i: usize, carry: usize, raw: &[u8]) -> u8 {
        if i < carry {
            self.leftover[i]
        } else {
            raw[i - carry]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dop_marker_pair_layout() {
        let mut f = DoPFramer::new();
        // 3 stereo frames: [L0,R0][L1,R1][L2,R2 exceeded?] — use 4 frames so
        // every pair is complete: [L0..L3][R0..R3].
        let raw = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let mut out = [0u32; 4];
        let n = f.frame(&raw, 2, &mut out);
        assert_eq!(n, 4); // L,R for pair0 + L,R for pair1
        // Pair 0: (older,newer) of L = (0x01,0x03), of R = (0x02,0x04), even marker.
        assert_eq!(out[0], ((DOP_MARKER_EVEN as u32) << 16) | (0x01 << 8) | 0x03);
        assert_eq!(out[1], ((DOP_MARKER_EVEN as u32) << 16) | (0x02 << 8) | 0x04);
        // Pair 1: phase toggled to odd, (L1, L3)=(0x05,0x07), (R1,R3)=(0x06,0x08).
        assert_eq!(out[2], ((DOP_MARKER_ODD as u32) << 16) | (0x05 << 8) | 0x07);
        assert_eq!(out[3], ((DOP_MARKER_ODD as u32) << 16) | (0x06 << 8) | 0x08);
        assert!(!f.marker_phase); // two emitted frames toggled odd -> even
    }

    #[test]
    fn dop_odd_tail_is_carried_across_calls() {
        let mut f = DoPFramer::new();
        f.with_channels(2);
        // 3 stereo frames (6 bytes) = 1 full pair (4 bytes) + 1 incomplete
        // frame (2 bytes) that has to be carried into the next call.
        let mut out = [0u32; 2];
        let n = f.frame(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66], 2, &mut out);
        assert_eq!(n, 2);
        assert_eq!(f.leftover.len(), 2); // the unpaired L2/R2 frame
        assert_eq!(out[0], ((DOP_MARKER_EVEN as u32) << 16) | (0x11 << 8) | 0x33);
        assert_eq!(out[1], ((DOP_MARKER_EVEN as u32) << 16) | (0x22 << 8) | 0x44);

        // The final single frame completes the carried pair and flips the phase.
        let mut out2 = [0u32; 2];
        let n2 = f.frame(&[0x77, 0x88], 2, &mut out2);
        assert_eq!(n2, 2);
        assert!(f.leftover.is_empty());
        assert_eq!(out2[0], ((DOP_MARKER_ODD as u32) << 16) | (0x55 << 8) | 0x77);
        assert_eq!(out2[1], ((DOP_MARKER_ODD as u32) << 16) | (0x66 << 8) | 0x88);
        assert!(!f.marker_phase); // two emitted frames toggled odd -> even
    }

    #[test]
    fn dop_phase_continues_across_calls() {
        let mut f = DoPFramer::new();
        f.with_channels(2);
        let mut out = [0u32; 4];
        let n = f.frame(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88], 2, &mut out);
        assert_eq!(n, 4); // 2 emitted frames -> phase back to even
        let mut out2 = [0u32; 2];
        // One more pair continues the sequence with an even marker...
        let n2 = f.frame(&[0x99, 0xAA, 0xBB, 0xCC], 2, &mut out2);
        assert_eq!(n2, 2);
        assert_eq!(out2[0], ((DOP_MARKER_EVEN as u32) << 16) | (0x99 << 8) | 0xBB);
        assert_eq!(out2[1], ((DOP_MARKER_EVEN as u32) << 16) | (0xAA << 8) | 0xCC);
        // ...and the phase persists into the next call as odd.
        let mut out3 = [0u32; 2];
        let n3 = f.frame(&[0x01, 0x02, 0x03, 0x04], 2, &mut out3);
        assert_eq!(n3, 2);
        assert_eq!(out3[0], ((DOP_MARKER_ODD as u32) << 16) | (0x01 << 8) | 0x03);
        assert_eq!(out3[1], ((DOP_MARKER_ODD as u32) << 16) | (0x02 << 8) | 0x04);
    }
}