//! Бейдж bit-perfect (AM1.0 §2.5, ADR-06).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use smallvec::SmallVec;

use crate::audio::backend::{AccessKind, DriverFormat};
use crate::audio::path::signal::SignalPath;
use crate::audio::path::transform::TransformKind;
use crate::audio::test_run::TestRecord;
use crate::settings::playback::Volume;

/// Причина «Не bit-perfect». Порядок вариантов — приоритет показа: первая
/// причина становится основной, остальные идут в признак «+N» (ADR-06, ТЗ-53, §2.5).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum NbpReason {
    /// Тест тракта не пройден.
    TestFailed,
    /// Драйвер не подтвердил запрошенные параметры (`hw_params`).
    DriverParamsUnconfirmed,
    /// DSD-источник выведен как PCM.
    DsdToPcm,
    /// Ресемплинг (SRC).
    Resample,
    /// Сведение каналов.
    Downmix,
    /// Усечение разрядности (вкл. float64 → float32, ОВС-1).
    Truncation,
    /// Квантование float-источника в целое (ОВС-1).
    FloatToInt,
    /// 32-битный целый источник переведён во float.
    Int32ToFloat,
    /// Программный mute стадии усиления.
    Mute,
    /// Программная громкость ≠ 100 %.
    Volume,
    /// Применён дизеринг.
    Dither,
}

/// Состояние усиления в момент вердикта (из SessionShared / PlaybackState, ОВС-15).
/// NotApplicable — стадия NoGain без mute (Строгий, Оптимальный с фиксацией, DoP);
/// Muted — mute любой стадией, включая ворота тишины NoGain/DopRender (ОВС-18).
/// Тот же снимок принимает `with_gain` для шагов 5–6 планировщика (§2.5, §6.3).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum GainState {
    NotApplicable,
    Unity,
    Scaled(Volume),
    Muted,
}

/// Состояние основного бейджа. Строится только функцией verdict() (§2.5, ADR-06).
///
/// `NoStream` функция `verdict` не возвращает — у неё всегда есть снимок пути;
/// это значение UI выставляет сам, когда поток не открыт или открытие не
/// удалось (ТЗ-56).
#[derive(Clone, PartialEq, Debug)]
pub enum BadgeState {
    /// Поток не открыт или последнее открытие не удалось (ТЗ-56).
    NoStream,
    Shared(SharedBadge),
    Exclusive(ExclusiveBadge),
}

/// Совместимый режим: «Bit-perfect» непредставим (ТЗ-54, решение 11).
#[derive(Clone, PartialEq, Debug)]
pub enum SharedBadge {
    /// «Shared: без изменений до системы».
    Clean,
    NotBitPerfect(NbpSummary),
}

/// Бейдж монопольного доступа (§2.5, ADR-06).
#[derive(Clone, PartialEq, Debug)]
pub enum ExclusiveBadge {
    /// Конструктор приватный: создаётся только в verdict() при passthrough
    /// и DriverFormat::Confirmed == запрошенному (ТЗ-52).
    BitPerfect(BitPerfectProof),
    NotBitPerfect(NbpSummary),
    /// «Mute» (ОВС-18): пока mute включён; после снятия verdict() без переоткрытия
    /// возвращает прежний результат.
    Muted,
}

/// Непубличное поле делает BitPerfect непостроимым вне модуля (ТЗ-52, ОВ-14).
#[derive(Clone, PartialEq, Debug)]
pub struct BitPerfectProof {
    _private: (),
}

/// Сводка причин «Не bit-perfect» (ТЗ-53, ADR-06).
#[derive(Clone, PartialEq, Debug)]
pub struct NbpSummary {
    pub primary: NbpReason,
    /// Число остальных причин для признака «+N» (ТЗ-53).
    pub more: u8,
}

/// Вердикт бейджа — чистая функция снимка, усиления и результата теста
/// (§6.22, ADR-06, ТЗ-52…ТЗ-58, ОВ-14, ОВС-18).
///
/// Причины собираются, сортируются в порядке `NbpReason` (ADR-06) и
/// дедуплицируются: Volume/Mute/Dither могут прийти и из преобразований пути,
/// и из `gain`. Underrun, xrun, MD5 и счётчики ограничений не участвуют (ТЗ-52).
/// `TestFailed` даёт только запись, ключ которой совпадает с путём
/// (`ConfigKey::matches_path`).
pub fn verdict(path: &SignalPath, gain: GainState, test: Option<&TestRecord>) -> BadgeState {
    let mut reasons: SmallVec<[NbpReason; 16]> = SmallVec::new();
    if let Some(rec) = test {
        if !rec.passed && rec.key.matches_path(path) {
            reasons.push(NbpReason::TestFailed);
        }
    }
    let exclusive = path.output.access == AccessKind::Exclusive;
    if exclusive && matches!(path.output.driver, DriverFormat::Unreadable { .. }) {
        reasons.push(NbpReason::DriverParamsUnconfirmed);
    }
    reasons.extend(path.lossy().filter_map(|t| t.kind.badge_reason()));
    match gain {
        GainState::Muted => reasons.push(NbpReason::Mute),
        GainState::Scaled(_) => reasons.push(NbpReason::Volume),
        GainState::Unity | GainState::NotApplicable => {}
    }
    if path.processing.transforms.iter().any(|t| matches!(t.kind, TransformKind::Dither(_))) {
        reasons.push(NbpReason::Dither);
    }
    reasons.sort_unstable();
    reasons.dedup();

    if !exclusive {
        return match summary(&reasons) {
            None => BadgeState::Shared(SharedBadge::Clean),
            Some(s) => BadgeState::Shared(SharedBadge::NotBitPerfect(s)),
        };
    }
    // ОВС-18: mute в Exclusive — отдельное состояние, не причина.
    if gain == GainState::Muted {
        return BadgeState::Exclusive(ExclusiveBadge::Muted);
    }
    let confirmed = matches!(path.output.driver, DriverFormat::Confirmed(f) if f == path.output.requested);
    match summary(&reasons) {
        None if confirmed => BadgeState::Exclusive(ExclusiveBadge::BitPerfect(BitPerfectProof { _private: () })),
        Some(s) => BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(s)),
        // Драйвер подтвердил формат, отличный от запрошенного (ADR-07): причины
        // в снимке нет, поэтому основная — «параметры драйвера не подтверждены».
        None => BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(NbpSummary {
            primary: NbpReason::DriverParamsUnconfirmed,
            more: 0,
        })),
    }
}

/// `summary(r) = NbpSummary { primary: r[0], more: r.len() − 1 }` (§6.22);
/// `None` для пустого списка. `more` насыщается на `u8::MAX`.
fn summary(reasons: &[NbpReason]) -> Option<NbpSummary> {
    let (&primary, rest) = reasons.split_first()?;
    Some(NbpSummary { primary, more: u8::try_from(rest.len()).unwrap_or(u8::MAX) })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use std::num::NonZeroU8;
    use std::time::SystemTime;

    use super::*;
    use crate::audio::backend::OutputApi;
    use crate::audio::format::{
        BitDepth, ChannelLayout, Codec, Container, OutputSampleFormat, SampleRate, SourceFormat, SourceKind,
        StreamFormat,
    };
    use crate::audio::path::transform::{Cause, Confidence, Transform};
    use crate::audio::path::{OpenedFacts, OsStage, PathPlan, ReservationView, Route};
    use crate::audio::session::RingPayload;
    use crate::audio::test_run::ConfigKey;
    use crate::settings::playback::{Dither, ModeKind};

    fn sr(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    fn fmt(hz: u32) -> StreamFormat {
        StreamFormat { rate: sr(hz), sample: OutputSampleFormat::S32Le, channels: NonZeroU8::new(2).unwrap() }
    }

    fn plan_with(transforms: &[TransformKind]) -> PathPlan {
        PathPlan {
            mode: ModeKind::Optimal,
            source: SourceFormat {
                container: Container::Flac,
                codec: Codec::Flac,
                lossy: false,
                rate: sr(44_100),
                layout: ChannelLayout::stereo(),
                kind: SourceKind::Pcm { bits: BitDepth::new(16).unwrap() },
            },
            route: Route::Pcm,
            transforms: transforms
                .iter()
                .cloned()
                .map(|kind| Transform { kind, cause: Cause::ModeRule, confidence: Confidence::Assumed })
                .collect(),
            target: fmt(44_100),
            ring: RingPayload::ExactI32 { valid_bits: BitDepth::new(16).unwrap() },
            dsd_chain: None,
        }
    }

    fn path(access: AccessKind, driver: DriverFormat, transforms: &[TransformKind]) -> SignalPath {
        let facts = OpenedFacts {
            access,
            api: OutputApi::AlsaHw,
            device_name: "hw:0".to_owned(),
            driver,
            reservation: ReservationView::Held,
            os: OsStage::NotInvolved,
            device_buffer: None,
        };
        SignalPath::from_opened(plan_with(transforms), facts, 1)
    }

    fn exclusive(transforms: &[TransformKind]) -> SignalPath {
        path(AccessKind::Exclusive, DriverFormat::Confirmed(fmt(44_100)), transforms)
    }

    fn nbp(primary: NbpReason, more: u8) -> NbpSummary {
        NbpSummary { primary, more }
    }

    fn bit_perfect() -> BadgeState {
        BadgeState::Exclusive(ExclusiveBadge::BitPerfect(BitPerfectProof { _private: () }))
    }

    fn key() -> ConfigKey {
        ConfigKey {
            device: "hw:0".to_owned(),
            mode: ModeKind::Optimal,
            rate: sr(44_100),
            format: OutputSampleFormat::S32Le,
            channels: 2,
            player_version: "0.1.0".to_owned(),
            kernel: "6.8.0".to_owned(),
        }
    }

    fn record(key: ConfigKey, passed: bool) -> TestRecord {
        TestRecord { key, passed, date: SystemTime::UNIX_EPOCH }
    }

    /// ТЗ-54, ТЗ-56: Shared — Clean без причин, иначе NotBitPerfect; нечитаемый
    /// драйвер в Shared причиной не является (§6.22); mute в Shared — причина.
    #[test]
    fn verdict_shared_clean_and_reasons() {
        let unreadable = DriverFormat::Unreadable { requested: fmt(44_100) };
        let clean = path(AccessKind::Shared, unreadable, &[]);
        assert_eq!(verdict(&clean, GainState::Unity, None), BadgeState::Shared(SharedBadge::Clean));
        let lossy = path(AccessKind::Shared, unreadable, &[TransformKind::Int32ToFloat]);
        let v = Volume::new(50).unwrap();
        assert_eq!(
            verdict(&lossy, GainState::Scaled(v), None),
            BadgeState::Shared(SharedBadge::NotBitPerfect(nbp(NbpReason::Int32ToFloat, 1)))
        );
        assert_eq!(
            verdict(&clean, GainState::Muted, None),
            BadgeState::Shared(SharedBadge::NotBitPerfect(nbp(NbpReason::Mute, 0)))
        );
    }

    /// ТЗ-52, ОВ-14: BitPerfect только при Confirmed == запрошенному.
    #[test]
    fn verdict_exclusive_bitperfect_requires_confirmed() {
        assert_eq!(verdict(&exclusive(&[]), GainState::NotApplicable, None), bit_perfect());
        assert_eq!(verdict(&exclusive(&[]), GainState::Unity, None), bit_perfect());
        let unreadable = path(AccessKind::Exclusive, DriverFormat::Unreadable { requested: fmt(44_100) }, &[]);
        assert_eq!(
            verdict(&unreadable, GainState::NotApplicable, None),
            BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(nbp(NbpReason::DriverParamsUnconfirmed, 0)))
        );
        let other = path(AccessKind::Exclusive, DriverFormat::Confirmed(fmt(48_000)), &[]);
        assert!(matches!(
            verdict(&other, GainState::NotApplicable, None),
            BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(_))
        ));
        let lossy = exclusive(&[TransformKind::FloatNarrow]);
        assert_eq!(
            verdict(&lossy, GainState::NotApplicable, None),
            BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(nbp(NbpReason::Truncation, 0)))
        );
    }

    /// ОВС-18: тот же снимок с Muted → «Mute», после снятия — прежний BitPerfect.
    #[test]
    fn verdict_exclusive_muted_then_restored() {
        let p = exclusive(&[TransformKind::SilenceGate]);
        assert_eq!(verdict(&p, GainState::Muted, None), BadgeState::Exclusive(ExclusiveBadge::Muted));
        assert_eq!(verdict(&p, GainState::NotApplicable, None), bit_perfect());
        // Mute перекрывает и причины «Не bit-perfect».
        let lossy = exclusive(&[TransformKind::Int32ToFloat]);
        assert_eq!(verdict(&lossy, GainState::Muted, None), BadgeState::Exclusive(ExclusiveBadge::Muted));
    }

    /// ADR-06, ТЗ-53: причины по приоритету, без дублей (Volume из пути и из gain), «+N».
    #[test]
    fn verdict_reasons_sorted_with_more() {
        let v = Volume::new(50).unwrap();
        let p = path(
            AccessKind::Exclusive,
            DriverFormat::Unreadable { requested: fmt(44_100) },
            &[
                TransformKind::Dither(Dither::Tpdf),
                TransformKind::Volume(v),
                TransformKind::FloatNarrow,
                TransformKind::Int32ToFloat,
            ],
        );
        // DriverParamsUnconfirmed, Truncation, Int32ToFloat, Volume, Dither.
        assert_eq!(
            verdict(&p, GainState::Scaled(v), None),
            BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(nbp(NbpReason::DriverParamsUnconfirmed, 4)))
        );
        assert_eq!(summary(&[]), None);
        let many = [NbpReason::Dither; 300];
        assert_eq!(summary(&many).map(|s| s.more), Some(u8::MAX));
    }

    /// §6.22: TestFailed — только непройденная запись с ключом, совпадающим с путём.
    #[test]
    fn verdict_test_failed_only_for_matching_key() {
        let p = exclusive(&[]);
        let failed = record(key(), false);
        assert_eq!(
            verdict(&p, GainState::NotApplicable, Some(&failed)),
            BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(nbp(NbpReason::TestFailed, 0)))
        );
        assert_eq!(verdict(&p, GainState::NotApplicable, Some(&record(key(), true))), bit_perfect());
        let other_device = ConfigKey { device: "hw:1".to_owned(), ..key() };
        assert_eq!(verdict(&p, GainState::NotApplicable, Some(&record(other_device, false))), bit_perfect());
        let other_rate = ConfigKey { rate: sr(48_000), ..key() };
        assert_eq!(verdict(&p, GainState::NotApplicable, Some(&record(other_rate, false))), bit_perfect());
        let other_mode = ConfigKey { mode: ModeKind::Strict, ..key() };
        assert_eq!(verdict(&p, GainState::NotApplicable, Some(&record(other_mode, false))), bit_perfect());
        // Версия плеера и ядра из снимка не выводятся (решение шага AM1.0-8.5).
        let other_kernel = ConfigKey { kernel: "6.9.0".to_owned(), ..key() };
        assert!(other_kernel.matches_path(&p));
    }
}
