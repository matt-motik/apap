//! Колбэк PCM: перевод формата, усиление, TPDF (§6.14, ТЗ-4…ТЗ-8, ТЗ-17, ТЗ-68,
//! ADR-03, ADR-06, И-Р7…И-Р9, И-Р19, И-Р23).
//!
//! `PcmRender<P, O, G>` мономорфизирован по типу ring `P`, формату выхода `O`
//! и стадии усиления `G`: ветвь выбирается один раз на период, в цикле по
//! сэмплам — только запись в байты.

use std::marker::PhantomData;

use super::gain::{GainStage, GainState};
use super::out::{OutFormat, EXACT_TO_UNIT};
use super::tpdf::Tpdf;
use super::{Period, RenderCore, RingSample};
use crate::audio::session::RingPayload;

/// Шум TPDF только для целых форматов: у `F32` квантования нет (ТЗ-8).
#[inline]
fn dither<O: OutFormat>(tpdf: &mut Tpdf) -> f64 {
    if O::IS_INT {
        tpdf.sample()
    } else {
        0.0
    }
}

/// Запись одного сэмпла ring в формат `O` по ветви усиления (таблица «Ветви усиления», §6.14).
pub trait PcmSample: RingSample {
    /// `exact` — ветвь `Unity` пишет исходные биты сдвигом (`ExactI32` при `vb ≤ N`
    /// или выход `F32`); иначе — TPDF и квантование.
    fn write<O: OutFormat>(self, dst: &mut [u8], g: GainState, exact: bool, tpdf: &mut Tpdf);
}

impl PcmSample for i32 {
    #[inline]
    fn write<O: OutFormat>(self, dst: &mut [u8], g: GainState, exact: bool, tpdf: &mut Tpdf) {
        match g {
            GainState::Unity if exact => O::write_exact(dst, self),
            GainState::Unity => {
                O::write_float(dst, f64::from(self) * EXACT_TO_UNIT, dither::<O>(tpdf));
            }
            GainState::Scaled(f) => {
                O::write_float(dst, f64::from(self) * EXACT_TO_UNIT * f, dither::<O>(tpdf));
            }
            GainState::Muted => O::write_silence(dst),
        }
    }
}

impl PcmSample for f32 {
    #[inline]
    fn write<O: OutFormat>(self, dst: &mut [u8], g: GainState, _exact: bool, tpdf: &mut Tpdf) {
        match g {
            GainState::Unity => O::write_float(dst, f64::from(self), dither::<O>(tpdf)),
            GainState::Scaled(f) => O::write_float(dst, f64::from(self) * f, dither::<O>(tpdf)),
            GainState::Muted => O::write_silence(dst),
        }
    }
}

/// Рендер PCM-периода поверх [`RenderCore`].
pub struct PcmRender<P: PcmSample, O: OutFormat, G: GainStage> {
    core: RenderCore<P>,
    tpdf: Tpdf,
    /// Ветвь `Unity` пишет исходные биты (вычислено при сборке из `RingPayload` и `O`).
    exact: bool,
    _fmt: PhantomData<fn() -> (O, G)>,
}

impl<P: PcmSample, O: OutFormat, G: GainStage> PcmRender<P, O, G> {
    /// `tpdf` фиксирован при сборке потока (`Tpdf::off()` для `Dither::Off`).
    /// `ExactI32(vb)` при `vb > N` — усечение через TPDF и квантование (только Оптимальный).
    pub fn new(core: RenderCore<P>, payload: RingPayload, tpdf: Tpdf) -> Self {
        let exact = match payload {
            RingPayload::ExactI32 { valid_bits } => {
                !O::IS_INT || u32::from(valid_bits.bits()) <= O::BITS
            }
            RingPayload::F32 => false,
        };
        Self {
            core,
            tpdf,
            exact,
            _fmt: PhantomData,
        }
    }

    pub fn core(&self) -> &RenderCore<P> {
        &self.core
    }

    /// Один период (§6.14). Хвост, не покрытый кадрами ring, — тишина формата.
    pub fn render(&mut self, out: &mut [u8]) {
        if self.core.begin() == Period::Silence {
            out.fill(0);
            return;
        }
        let frame_bytes = O::BYTES.saturating_mul(self.core.channels());
        let need = out.len().checked_div(frame_bytes).unwrap_or(0);
        let g = G::load(self.core.shared());
        let exact = self.exact;
        let tpdf = &mut self.tpdf;

        let mut written_samples = 0usize;
        if let Some(chunk) = self.core.read(need) {
            let (a, b) = chunk.as_slices();
            // `as_chunks_mut::<{ O::BYTES }>` недоступен для обобщённого `O` (generic_const_exprs).
            #[allow(clippy::chunks_exact_to_as_chunks)]
            for (&s, dst) in a.iter().chain(b).zip(out.chunks_exact_mut(O::BYTES)) {
                s.write::<O>(dst, g, exact, tpdf);
                written_samples = written_samples.saturating_add(1);
            }
            chunk.commit_all();
        }
        let written_bytes = written_samples.saturating_mul(O::BYTES);
        out.iter_mut().skip(written_bytes).for_each(|b| *b = 0);

        let written = written_samples
            .checked_div(self.core.channels())
            .unwrap_or(0);
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
    use crate::audio::format::BitDepth;
    use crate::audio::render::gain::{AtomicGain, NoGain};
    use crate::audio::render::out::{F32Le, S16Le, S24Le, S24_3Le, S32Le};
    use crate::audio::session::SessionShared;
    use rtrb::{Producer, RingBuffer};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    const CH: usize = 2;

    fn exact(bits: u8) -> RingPayload {
        RingPayload::ExactI32 {
            valid_bits: BitDepth::new(bits).unwrap(),
        }
    }

    fn render<P: PcmSample, O: OutFormat, G: GainStage>(
        payload: RingPayload,
        tpdf: Tpdf,
        prime: usize,
    ) -> (Producer<P>, PcmRender<P, O, G>) {
        let (p, c) = RingBuffer::<P>::new(1 << 16);
        let core = RenderCore::new(c, Arc::new(SessionShared::new()), CH, prime);
        core.shared().playing.store(true, Ordering::Release);
        (p, PcmRender::new(core, payload, tpdf))
    }

    fn period<O: OutFormat>(frames: usize) -> Vec<u8> {
        vec![0xAA; frames * CH * O::BYTES]
    }

    fn read_int<O: OutFormat>(b: &[u8]) -> i32 {
        let mut w = [0u8; 4];
        w[..O::BYTES].copy_from_slice(b);
        let unused = 32 - 8 * O::BYTES as u32;
        (i32::from_le_bytes(w) << unused) >> unused
    }

    /// Unity + ExactI32(vb ≤ N): выход — исходные биты (И-Р19), дизеринг включён.
    fn unity_bit_exact<O: OutFormat, G: GainStage>(src_bits: u8, vals: &[i32]) {
        let (mut p, mut r) = render::<i32, O, G>(exact(src_bits), Tpdf::new(), 0);
        for &v in vals {
            p.push(v << (32 - u32::from(src_bits))).unwrap();
        }
        let mut out = period::<O>(vals.len() / CH);
        r.render(&mut out);
        for (dst, &v) in out.chunks_exact(O::BYTES).zip(vals) {
            let back = read_int::<O>(dst) << (32 - O::BITS);
            assert_eq!(back, v << (32 - u32::from(src_bits)));
        }
    }

    const V16: [i32; 6] = [i16::MIN as i32, i16::MAX as i32, 0, -1, 1, 0x1234];
    const V24: [i32; 6] = [-8_388_608, 8_388_607, 0, -1, 0x12_3456, -0x55_AAAA];
    const V32: [i32; 6] = [i32::MIN, i32::MAX, 0xFF, -0x81, 0x1234_5678, -1];

    #[test]
    fn optimal_unity_gain_is_bit_exact_s16() {
        unity_bit_exact::<S16Le, AtomicGain>(16, &V16);
    }

    #[test]
    fn optimal_unity_gain_is_bit_exact_s24_3le() {
        unity_bit_exact::<S24_3Le, AtomicGain>(24, &V24);
    }

    #[test]
    fn optimal_unity_gain_is_bit_exact_s24le() {
        unity_bit_exact::<S24Le, AtomicGain>(24, &V24);
    }

    #[test]
    fn optimal_unity_gain_is_bit_exact_s32() {
        unity_bit_exact::<S32Le, AtomicGain>(32, &V32);
    }

    #[test]
    fn passthrough_strict_is_bit_exact() {
        unity_bit_exact::<S32Le, NoGain>(32, &V32);
        unity_bit_exact::<S24_3Le, NoGain>(24, &V24);
    }

    #[test]
    fn dither_not_applied_on_zero_pad() {
        unity_bit_exact::<S24_3Le, AtomicGain>(16, &V16);
        unity_bit_exact::<S32Le, AtomicGain>(16, &V16);
        unity_bit_exact::<S32Le, AtomicGain>(24, &V24);
    }

    #[test]
    fn dither_applied_after_resample() {
        // F32 → S16 с TPDF: шум в пределах ±1 LSB, хотя бы часть сэмплов изменена.
        let frames = 512;
        let (mut p, mut r) = render::<f32, S16Le, AtomicGain>(RingPayload::F32, Tpdf::new(), 0);
        let (mut p0, mut r0) = render::<f32, S16Le, AtomicGain>(RingPayload::F32, Tpdf::off(), 0);
        let x = 1000.25f32 / 32768.0;
        for _ in 0..frames * CH {
            p.push(x).unwrap();
            p0.push(x).unwrap();
        }
        let (mut out, mut out0) = (period::<S16Le>(frames), period::<S16Le>(frames));
        r.render(&mut out);
        r0.render(&mut out0);
        let mut changed = 0;
        for (d, d0) in out.chunks_exact(2).zip(out0.chunks_exact(2)) {
            let (q, q0) = (read_int::<S16Le>(d), read_int::<S16Le>(d0));
            assert_eq!(q0, 1000, "Dither::Off — точное округление");
            assert!((q - q0).abs() <= 1);
            changed += usize::from(q != q0);
        }
        assert!(changed > 0);
    }

    #[test]
    fn truncation_uses_quantize_with_dither() {
        // ExactI32(24) → S16 (vb > N): квантование, без паники и в пределах ±1 LSB.
        let (mut p, mut r) = render::<i32, S16Le, AtomicGain>(exact(24), Tpdf::new(), 0);
        for v in [0x12_3456i32, -0x12_3456] {
            p.push(v << 8).unwrap();
        }
        let mut out = period::<S16Le>(1);
        r.render(&mut out);
        let l = read_int::<S16Le>(&out[0..2]);
        let rr = read_int::<S16Le>(&out[2..4]);
        assert!((l - 0x1234).abs() <= 1);
        assert!((rr + 0x1234).abs() <= 1);
    }

    #[test]
    fn scaled_gain_halves_f32_out() {
        let (mut p, mut r) = render::<i32, F32Le, AtomicGain>(exact(24), Tpdf::new(), 0);
        r.core().shared().set_gain(0.5);
        p.push(0x40_0000 << 8).unwrap();
        p.push(0).unwrap();
        let mut out = period::<F32Le>(1);
        r.render(&mut out);
        assert_eq!(f32::from_le_bytes([out[0], out[1], out[2], out[3]]), 0.25);
    }

    #[test]
    fn underrun_counted_only_when_playing() {
        let (mut p, mut r) = render::<i32, S16Le, AtomicGain>(exact(16), Tpdf::off(), 2);
        // Пауза: тишина, underrun нет.
        r.core().shared().playing.store(false, Ordering::Release);
        let mut out = period::<S16Le>(4);
        r.render(&mut out);
        assert!(out.iter().all(|&b| b == 0));
        assert_eq!(r.core().shared().underruns.load(Ordering::Relaxed), 0);
        // Набор запаса: тишина, underrun нет, позиция стоит.
        r.core().shared().playing.store(true, Ordering::Release);
        r.render(&mut out);
        assert_eq!(r.core().shared().underruns.load(Ordering::Relaxed), 0);
        assert_eq!(r.core().shared().pos_frames.load(Ordering::Relaxed), 0);
        // Запас набран, но кадров меньше периода: underrun.
        for v in 1..=4 {
            p.push(v << 16).unwrap();
        }
        r.render(&mut out);
        assert_eq!(r.core().shared().pos_frames.load(Ordering::Relaxed), 2);
        assert_eq!(r.core().shared().underruns.load(Ordering::Relaxed), 1);
        assert_eq!(read_int::<S16Le>(&out[6..8]), 4);
        assert!(out[8..].iter().all(|&b| b == 0), "хвост — тишина");
    }

    #[test]
    fn priming_ends_at_eof_short_track() {
        let (mut p, mut r) = render::<i32, S16Le, AtomicGain>(exact(16), Tpdf::off(), 1000);
        p.push(7 << 16).unwrap();
        p.push(8 << 16).unwrap();
        r.core().shared().eof_frame.store(1, Ordering::Release);
        let mut out = period::<S16Le>(4);
        r.render(&mut out);
        assert_eq!(read_int::<S16Le>(&out[0..2]), 7);
        assert_eq!(r.core().shared().underruns.load(Ordering::Relaxed), 0);
        assert!(r.core().shared().ended.load(Ordering::Acquire));
    }

    #[test]
    fn strict_mute_outputs_exact_bits_or_silence() {
        let (mut p, mut r) = render::<i32, S24_3Le, NoGain>(exact(24), Tpdf::new(), 0);
        for &v in &V24 {
            p.push(v << 8).unwrap();
        }
        r.core().shared().muted.store(true, Ordering::Relaxed);
        let mut out = period::<S24_3Le>(1);
        r.render(&mut out);
        assert!(out.iter().all(|&b| b == 0), "mute — нули формата");
        assert_eq!(r.core().shared().pos_frames.load(Ordering::Relaxed), 1);
        r.core().shared().muted.store(false, Ordering::Relaxed);
        r.render(&mut out);
        assert_eq!(read_int::<S24_3Le>(&out[0..3]), V24[2]);
        assert_eq!(read_int::<S24_3Le>(&out[3..6]), V24[3]);
    }
}
