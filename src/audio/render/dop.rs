//! Колбэк DoP (§6.15, ТЗ-1…ТЗ-3, ТЗ-108, ADR-12, И-Р3, И-Р25).
//!
//! Фаза маркера общая для всех каналов кадра и переключается на каждом кадре
//! независимо от источника нагрузки: пауза, seek, стартовый запас, underrun и
//! mute её не нарушают (ТЗ-2). Стадии усиления и tap визуализатора нет (И-Т6).

use std::marker::PhantomData;
use std::sync::atomic::Ordering;

use super::out::OutFormat;
use super::{Period, RenderCore};
use crate::audio::dop::{DOP_MARKER_EVEN, DOP_MARKER_ODD};

/// Байт DSD-тишины: нагрузка `0x6969` (ТЗ-3).
const DSD_SILENCE: u8 = 0x69;

/// 32-битное DoP-слово `(m << 24) | (payload << 8)`; формат `O` пишет его
/// сдвигом `>> (32 − N)`: `S24_3LE` → `[lo, hi, m]`, `S24_LE` → `[lo, hi, m, sign(m)]`,
/// `S32_LE` → `[0, lo, hi, m]`.
#[inline]
fn dop_word(marker: u8, hi: u8, lo: u8) -> i32 {
    i32::from_be_bytes([marker, hi, lo, 0])
}

/// Заполняет `out` целыми кадрами: маркер по фазе, нагрузка — биты 23..8 из
/// `samples` или `0x6969`, когда сэмплов нет или `silence`. Возвращает число
/// потреблённых из `samples` сэмплов (при mute они потребляются, ОВС-18).
fn fill_frames<O: OutFormat>(
    out: &mut [u8],
    channels: usize,
    phase: &mut bool,
    samples: &mut impl Iterator<Item = i32>,
    silence: bool,
) -> usize {
    let frame_bytes = O::BYTES.saturating_mul(channels);
    let mut consumed = 0usize;
    if frame_bytes == 0 {
        return 0;
    }
    // `as_chunks_mut::<{ O::BYTES }>` недоступен для обобщённого `O` (generic_const_exprs).
    #[allow(clippy::chunks_exact_to_as_chunks)]
    let mut frames = out.chunks_exact_mut(frame_bytes);
    for frame in &mut frames {
        let marker = if *phase {
            DOP_MARKER_ODD
        } else {
            DOP_MARKER_EVEN
        };
        *phase = !*phase;
        #[allow(clippy::chunks_exact_to_as_chunks)]
        for dst in frame.chunks_exact_mut(O::BYTES) {
            let (hi, lo) = match samples.next() {
                Some(s) => {
                    consumed = consumed.saturating_add(1);
                    if silence {
                        (DSD_SILENCE, DSD_SILENCE)
                    } else {
                        let [_, lo, hi, _] = s.to_le_bytes();
                        (hi, lo)
                    }
                }
                None => (DSD_SILENCE, DSD_SILENCE),
            };
            O::write_exact(dst, dop_word(marker, hi, lo));
        }
    }
    frames.into_remainder().fill(0);
    consumed
}

/// Рендер DoP-периода поверх [`RenderCore`]; ring — `ExactI32` с нагрузкой в битах 23..8.
/// `O` — целый формат не уже 24 бит (`S24_3LE`, `S24_LE`, `S32_LE`).
pub struct DopRender<O: OutFormat> {
    core: RenderCore<i32>,
    /// `false` — следующий кадр с маркером `0x05`, `true` — `0xFA`.
    marker_phase: bool,
    _fmt: PhantomData<fn() -> O>,
}

impl<O: OutFormat> DopRender<O> {
    pub fn new(core: RenderCore<i32>) -> Self {
        Self {
            core,
            marker_phase: false,
            _fmt: PhantomData,
        }
    }

    pub fn core(&self) -> &RenderCore<i32> {
        &self.core
    }

    /// Один период (§6.15). Тишина паузы, seek, priming и хвост underrun — DoP-тишина с маркерами.
    pub fn render(&mut self, out: &mut [u8]) {
        let channels = self.core.channels();
        if self.core.begin() == Period::Silence {
            fill_frames::<O>(
                out,
                channels,
                &mut self.marker_phase,
                &mut core::iter::empty(),
                true,
            );
            return;
        }
        let frame_bytes = O::BYTES.saturating_mul(channels);
        let need = out.len().checked_div(frame_bytes).unwrap_or(0);
        let muted = self.core.shared().muted.load(Ordering::Relaxed);
        let phase = &mut self.marker_phase;

        let consumed = match self.core.read(need) {
            Some(chunk) => {
                let (a, b) = chunk.as_slices();
                let n =
                    fill_frames::<O>(out, channels, phase, &mut a.iter().chain(b).copied(), muted);
                chunk.commit_all();
                n
            }
            None => fill_frames::<O>(out, channels, phase, &mut core::iter::empty(), true),
        };
        let written = consumed.checked_div(channels).unwrap_or(0);
        self.core.finish(written, need);
    }
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::chunks_exact_to_as_chunks
)]
mod tests {
    use super::*;
    use crate::audio::render::out::{S24Le, S24_3Le, S32Le};
    use crate::audio::session::SessionShared;
    use rtrb::{Producer, RingBuffer};
    use std::sync::Arc;

    const CH: usize = 2;
    /// Нечётный период: фаза должна переноситься между периодами.
    const PERIOD: usize = 3;

    fn dop<O: OutFormat>(prime: usize) -> (Producer<i32>, DopRender<O>) {
        let (p, c) = RingBuffer::<i32>::new(1 << 12);
        let core = RenderCore::new(c, Arc::new(SessionShared::new()), CH, prime);
        core.shared().playing.store(true, Ordering::Release);
        (p, DopRender::new(core))
    }

    /// DSD-нагрузка `hi:lo` в битах 23..8, как пишет декодер.
    fn push_payload(p: &mut Producer<i32>, frames: usize) {
        for i in 0..frames * CH {
            p.push(((0x1200 + i as i32) & 0xFFFF) << 8).unwrap();
        }
    }

    /// (маркер, нагрузка) каждого сэмпла `S24_3LE`.
    fn words(out: &[u8]) -> Vec<(u8, u16)> {
        out.chunks_exact(3)
            .map(|b| (b[2], u16::from_le_bytes([b[0], b[1]])))
            .collect()
    }

    fn period(r: &mut DopRender<S24_3Le>) -> Vec<(u8, u16)> {
        let mut out = vec![0xAA; PERIOD * CH * 3];
        r.render(&mut out);
        words(&out)
    }

    fn assert_markers_continuous(all: &[(u8, u16)]) {
        let frames: Vec<u8> = all
            .chunks_exact(CH)
            .map(|f| {
                assert!(
                    f.iter().all(|w| w.0 == f[0].0),
                    "маркер общий для каналов кадра"
                );
                f[0].0
            })
            .collect();
        assert_eq!(frames[0], DOP_MARKER_EVEN);
        for w in frames.windows(2) {
            assert!(w[0] == DOP_MARKER_EVEN || w[0] == DOP_MARKER_ODD);
            assert_ne!(w[0], w[1], "соседние кадры — разные маркеры");
        }
    }

    #[test]
    fn dop_word_layout_per_format() {
        let w = dop_word(DOP_MARKER_ODD, 0x12, 0x34);
        let mut b3 = [0u8; 3];
        S24_3Le::write_exact(&mut b3, w);
        assert_eq!(b3, [0x34, 0x12, 0xFA]);
        let mut b4 = [0u8; 4];
        S24Le::write_exact(&mut b4, w);
        assert_eq!(b4, [0x34, 0x12, 0xFA, 0xFF]);
        S24Le::write_exact(&mut b4, dop_word(DOP_MARKER_EVEN, 0x12, 0x34));
        assert_eq!(b4, [0x34, 0x12, 0x05, 0x00]);
        S32Le::write_exact(&mut b4, w);
        assert_eq!(b4, [0x00, 0x34, 0x12, 0xFA]);
    }

    #[test]
    fn dop_payload_passes_bits_23_8() {
        let (mut p, mut r) = dop::<S24_3Le>(0);
        push_payload(&mut p, PERIOD);
        let w = period(&mut r);
        let payloads: Vec<u16> = w.iter().map(|x| x.1).collect();
        assert_eq!(
            payloads,
            (0x1200..0x1200 + (PERIOD * CH) as u16).collect::<Vec<_>>()
        );
        assert_markers_continuous(&w);
    }

    #[test]
    fn priming_dop_emits_marked_silence() {
        let (mut p, mut r) = dop::<S24_3Le>(PERIOD * 2);
        push_payload(&mut p, PERIOD);
        let n = r.core().shared().request_seek(100);
        let mut all = period(&mut r); // ожидание фазы 2
        r.core()
            .shared()
            .decoder_stopped
            .store(n, Ordering::Release);
        all.extend(period(&mut r)); // фаза 3: сброс ring
        push_payload(&mut p, PERIOD);
        all.extend(period(&mut r)); // priming: запас ещё не набран
        assert!(r.core().is_priming());
        assert!(all.iter().all(|w| w.1 == 0x6969));
        assert_markers_continuous(&all);
        assert_eq!(r.core().shared().underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn dop_silence_payload_is_6969() {
        let (mut p, mut r) = dop::<S24_3Le>(0);
        // Пауза.
        r.core().shared().playing.store(false, Ordering::Release);
        assert!(period(&mut r).iter().all(|w| w.1 == 0x6969));
        // Underrun: один кадр данных, хвост — DSD-тишина.
        r.core().shared().playing.store(true, Ordering::Release);
        push_payload(&mut p, 1);
        let w = period(&mut r);
        assert_ne!(w[0].1, 0x6969);
        assert!(w[CH..].iter().all(|x| x.1 == 0x6969));
        assert_eq!(r.core().shared().underruns.load(Ordering::Relaxed), 1);
        // Seek.
        r.core().shared().request_seek(5);
        assert!(period(&mut r).iter().all(|w| w.1 == 0x6969));
    }

    #[test]
    fn dop_markers_continuous_through_pause_seek_underrun() {
        let (mut p, mut r) = dop::<S24_3Le>(PERIOD);
        let s = Arc::clone(&r.core.shared);
        let mut all = Vec::new();
        push_payload(&mut p, PERIOD * 2);
        all.extend(period(&mut r)); // воспроизведение
        s.playing.store(false, Ordering::Release);
        all.extend(period(&mut r)); // пауза
        s.playing.store(true, Ordering::Release);
        all.extend(period(&mut r)); // снова воспроизведение
        let n = s.request_seek(0);
        all.extend(period(&mut r)); // ожидание seek
        s.decoder_stopped.store(n, Ordering::Release);
        all.extend(period(&mut r)); // сброс
        push_payload(&mut p, PERIOD);
        all.extend(period(&mut r)); // priming набран — воспроизведение
        all.extend(period(&mut r)); // underrun
        s.muted.store(true, Ordering::Relaxed);
        push_payload(&mut p, PERIOD);
        all.extend(period(&mut r)); // mute
        assert_markers_continuous(&all);
        assert_eq!(s.underruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn dop_mute_consumes_ring_with_silence_payload() {
        let (mut p, mut r) = dop::<S24_3Le>(0);
        push_payload(&mut p, PERIOD * 2);
        r.core().shared().muted.store(true, Ordering::Relaxed);
        assert!(period(&mut r).iter().all(|w| w.1 == 0x6969));
        assert_eq!(
            r.core().shared().pos_frames.load(Ordering::Relaxed),
            PERIOD as u64
        );
        assert_eq!(r.core().available_frames(), PERIOD);
        r.core().shared().muted.store(false, Ordering::Relaxed);
        assert_eq!(period(&mut r)[0].1, 0x1200 + (PERIOD * CH) as u16);
    }
}
