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

    use smallvec::SmallVec;

    use super::*;
    use crate::audio::backend::{DeviceCaps, OutputApi, RateSet};
    use crate::audio::format::{
        BitDepth, ChannelLayout, ChannelPos, Codec, Container, DsdRate, OutputSampleFormat, SampleRate, SourceFormat,
        SourceKind, StreamFormat,
    };
    use crate::audio::path::plan::with_gain;
    use crate::audio::path::signal::{DynamicPathState, Md5Status};
    use crate::audio::path::transform::{Cause, Confidence, Transform, Truncate, ZeroPad};
    use crate::audio::path::{plan, OpenedFacts, OsStage, PathPlan, ReservationView, Route};
    use crate::audio::session::RingPayload;
    use crate::audio::test_run::ConfigKey;
    use crate::settings::playback::{Dither, DsdFilter, ModeKind, ModeSettings, PathPolicy, SrcFilter};

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

    /// Снимок пути для плана `plan` после открытия с доступом `access` и
    /// ответом драйвера `driver` (§3.1).
    fn opened(plan: PathPlan, access: AccessKind, driver: DriverFormat) -> SignalPath {
        let facts = OpenedFacts {
            access,
            api: OutputApi::AlsaHw,
            device_name: "hw:0".to_owned(),
            driver,
            reservation: ReservationView::Held,
            os: OsStage::NotInvolved,
            device_buffer: None,
        };
        SignalPath::from_opened(plan, facts, 1)
    }

    fn path(access: AccessKind, driver: DriverFormat, transforms: &[TransformKind]) -> SignalPath {
        opened(plan_with(transforms), access, driver)
    }

    /// Exclusive, драйвер подтвердил ровно запрошенный формат (цель плана).
    fn confirmed(plan: PathPlan) -> SignalPath {
        let f = plan.target;
        opened(plan, AccessKind::Exclusive, DriverFormat::Confirmed(f))
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

    // -----------------------------------------------------------------------
    // Тесты бейджа §7.2 (ТЗ-52, ТЗ-53), часть 1
    // -----------------------------------------------------------------------

    fn rates(hz: &[u32]) -> RateSet {
        RateSet::Discrete(hz.iter().map(|&h| sr(h)).collect())
    }

    fn bd(bits: u8) -> BitDepth {
        BitDepth::new(bits).unwrap()
    }

    fn vol(v: u8) -> Volume {
        Volume::new(v).unwrap()
    }

    /// Раскладка ITU 5.1: FL FR FC LFE BL BR (§6.13).
    fn layout_51() -> ChannelLayout {
        use ChannelPos::{BL, BR, FC, FL, FR, LFE};
        ChannelLayout::new(&[FL, FR, FC, LFE, BL, BR]).unwrap()
    }

    /// PCM-источник FLAC: `bits` бит, `rate` Гц, 1, 2 или 6 (5.1) каналов.
    fn src_pcm(bits: u8, rate: u32, ch: u8) -> SourceFormat {
        let layout = match ch {
            1 => ChannelLayout::mono(),
            2 => ChannelLayout::stereo(),
            6 => layout_51(),
            n => panic!("раскладка на {n} каналов в тестах вердикта не нужна"),
        };
        SourceFormat {
            container: Container::Flac,
            codec: Codec::Flac,
            lossy: false,
            rate: sr(rate),
            layout,
            kind: SourceKind::Pcm { bits: bd(bits) },
        }
    }

    /// DSD-источник DSF стерео: частота — частота DSD-битов на канал.
    fn src_dsd(d: DsdRate) -> SourceFormat {
        SourceFormat {
            container: Container::Dsf,
            codec: Codec::Dsf,
            lossy: false,
            rate: sr(d.bit_rate_hz()),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::Dsd { dsd: d },
        }
    }

    fn caps(access: AccessKind, api: OutputApi, hz: &[u32], formats: &[OutputSampleFormat]) -> DeviceCaps {
        DeviceCaps {
            api,
            access,
            name: "test".to_owned(),
            rates: rates(hz),
            formats: SmallVec::from_slice(formats),
            channels: SmallVec::from_slice(&[2]),
        }
    }

    /// Exclusive-устройство стерео (Оптимальный и Строгий режимы).
    fn caps_excl(hz: &[u32], formats: &[OutputSampleFormat]) -> DeviceCaps {
        caps(AccessKind::Exclusive, OutputApi::AlsaHw, hz, formats)
    }

    /// Реальный план `plan()` с шагами 5–6 (`with_gain`) для снимка `gain` (§6.3).
    fn real_plan(src: &SourceFormat, c: &DeviceCaps, policy: PathPolicy<'_>, gain: GainState) -> PathPlan {
        let p = plan(src, c, policy).unwrap_or_else(|b| panic!("план заблокирован: {b:?}"));
        with_gain(&p, gain, policy)
    }

    fn excl_nbp(primary: NbpReason, more: u8) -> BadgeState {
        BadgeState::Exclusive(ExclusiveBadge::NotBitPerfect(nbp(primary, more)))
    }

    /// Минимальный путь Exclusive с одним преобразованием `kind` (Confirmed).
    fn one(kind: TransformKind) -> SignalPath {
        exclusive(&[kind])
    }

    /// §7.2, ТЗ-52: таблица честности (строки таблицы §4 ревью) — режим × gain ×
    /// драйвер × преобразования → `BadgeState` по §6.22. Планы — из `plan()` и
    /// `with_gain()`, а не литералом.
    ///
    /// Строки «16 на I16/I32» в ревью описывают прежний дефект квантования
    /// (R-3). В новой модели ring `ExactI32` побитов, `ZeroPad` причины не даёт
    /// (§6.22), поэтому 16 бит на I16/I32 без регулировки — «Bit-perfect», а с
    /// громкостью < 100 % — «Не bit-perfect».
    ///
    /// Строка «неудачное открытие» сюда не входит: `verdict` всегда получает
    /// снимок пути, `NoStream` выставляет UI (ТЗ-56) — это тест
    /// `badge_no_stream_after_failed_open`.
    #[test]
    fn badge_honesty_table() {
        use OutputSampleFormat::{S16Le, S24Le, S32Le, F32};

        let s = ModeSettings::default();
        let strict = PathPolicy::Strict(&s.strict);
        let optimal = PathPolicy::Optimal(&s.optimal);
        let compatible = PathPolicy::Compatible(&s.compatible);
        let na = GainState::NotApplicable;
        let unity = GainState::Unity;
        let v80 = GainState::Scaled(vol(80));

        // (строка, путь, gain, ожидаемый бейдж)
        let mut rows: Vec<(&str, SignalPath, GainState, BadgeState)> = Vec::new();

        // strict_passthrough_24_192 (ТЗ-35): Строгий 24/192 на устройстве с
        // 192 кГц и S24_LE, драйвер подтвердил тот же формат.
        let dev_192 = caps_excl(&[96_000, 192_000], &[S24Le, S32Le]);
        for g in [na, unity] {
            let p = real_plan(&src_pcm(24, 192_000, 2), &dev_192, strict, g);
            assert!(p.transforms.is_empty(), "strict_passthrough_24_192: {:?}", p.transforms);
            assert_eq!(p.target.rate, sr(192_000));
            assert_eq!(p.target.sample, S24Le);
            rows.push(("strict_passthrough_24_192", confirmed(p), g, bit_perfect()));
        }

        // Усечение 24 → 16 (устройство только I16): Truncate + TPDF.
        let dev_16 = caps_excl(&[44_100], &[S16Le]);
        let p = real_plan(&src_pcm(24, 44_100, 2), &dev_16, optimal, unity);
        rows.push(("усечение 24→16", confirmed(p), unity, excl_nbp(NbpReason::Truncation, 1)));

        // 16 бит на I16 и на I32 (R-3): без регулировки — побитово.
        let dev_32 = caps_excl(&[44_100], &[S32Le]);
        for (name, dev) in [("16 на I16", &dev_16), ("16 на I32", &dev_32)] {
            let p = real_plan(&src_pcm(16, 44_100, 2), dev, strict, na);
            rows.push((name, confirmed(p), na, bit_perfect()));
            let p = real_plan(&src_pcm(16, 44_100, 2), dev, optimal, unity);
            rows.push((name, confirmed(p), unity, bit_perfect()));
        }

        // Громкость < 100 %: Volume + TPDF (целый выход переквантуется).
        let p = real_plan(&src_pcm(16, 44_100, 2), &dev_16, optimal, v80);
        rows.push(("громкость 80 %", confirmed(p), v80, excl_nbp(NbpReason::Volume, 1)));

        // Shared: «Bit-perfect» непредставим даже без преобразований (ТЗ-54).
        let server = caps(AccessKind::Shared, OutputApi::WasapiShared, &[96_000], &[F32]);
        let p = real_plan(&src_pcm(24, 96_000, 2), &server, compatible, unity);
        let f = p.target;
        let shared = opened(p, AccessKind::Shared, DriverFormat::Confirmed(f));
        rows.push(("Shared", shared, unity, BadgeState::Shared(SharedBadge::Clean)));

        // DSD→PCM после отказа DoP (нет 176,4 кГц) и DSD512 выше ЦАП: DsdToPcm + TPDF.
        let dev_no_dop = caps_excl(&[44_100, 88_200], &[S24Le, S32Le]);
        let p = real_plan(&src_dsd(DsdRate::Dsd64), &dev_no_dop, optimal, unity);
        rows.push(("DSD→PCM после DoP", confirmed(p), unity, excl_nbp(NbpReason::DsdToPcm, 1)));
        let dev_dop256 = caps_excl(&[176_400, 352_800, 705_600], &[S24Le, S32Le]);
        let p = real_plan(&src_dsd(DsdRate::Dsd512), &dev_dop256, optimal, unity);
        rows.push(("DSD512 выше ЦАП", confirmed(p), unity, excl_nbp(NbpReason::DsdToPcm, 1)));
        // Контраст: DoP — DopPack причины не даёт (§6.22).
        let p = real_plan(&src_dsd(DsdRate::Dsd64), &dev_dop256, optimal, na);
        assert!(matches!(p.route, Route::Dop(_)), "ожидался DoP: {:?}", p.route);
        rows.push(("DoP DSD64", confirmed(p), na, bit_perfect()));

        // Ресемплинг: 44,1 кГц на устройстве только с 48/96 кГц.
        let dev_48 = caps_excl(&[48_000, 96_000], &[S32Le]);
        let p = real_plan(&src_pcm(16, 44_100, 2), &dev_48, optimal, unity);
        rows.push(("SRC", confirmed(p), unity, excl_nbp(NbpReason::Resample, 1)));

        // Downmix 5.1 → 2.0 на стереоустройстве.
        let dev_st = caps_excl(&[48_000], &[S24Le]);
        let p = real_plan(&src_pcm(24, 48_000, 6), &dev_st, optimal, unity);
        rows.push(("downmix 5.1", confirmed(p), unity, excl_nbp(NbpReason::Downmix, 1)));

        for (name, p, g, want) in &rows {
            assert_eq!(&verdict(p, *g, None), want, "{name}: {:?}", p.processing.transforms);
        }
    }

    /// §7.2, ТЗ-52, ОВ-14: `Unreadable` в Exclusive → «Не bit-perfect: параметры
    /// драйвера не подтверждены». `Confirmed(f)` при `f ≠ requested` без иных
    /// причин — та же причина (решение шага 15 для пробела §6.22).
    #[test]
    fn badge_driver_unconfirmed() {
        use OutputSampleFormat::{S24Le, S32Le};

        let s = ModeSettings::default();
        let strict = PathPolicy::Strict(&s.strict);
        let dev = caps_excl(&[96_000, 192_000], &[S24Le, S32Le]);
        let pass = || real_plan(&src_pcm(24, 192_000, 2), &dev, strict, GainState::NotApplicable);
        let requested = pass().target;
        let unconfirmed = excl_nbp(NbpReason::DriverParamsUnconfirmed, 0);

        let p = opened(pass(), AccessKind::Exclusive, DriverFormat::Unreadable { requested });
        assert_eq!(verdict(&p, GainState::NotApplicable, None), unconfirmed);
        assert_eq!(verdict(&p, GainState::Unity, None), unconfirmed);

        // Подтверждён другой формат: частота, разрядность, каналы.
        let others = [
            StreamFormat { rate: sr(96_000), ..requested },
            StreamFormat { sample: S32Le, ..requested },
            StreamFormat { channels: NonZeroU8::new(4).unwrap(), ..requested },
        ];
        for f in others {
            let p = opened(pass(), AccessKind::Exclusive, DriverFormat::Confirmed(f));
            assert_eq!(verdict(&p, GainState::NotApplicable, None), unconfirmed, "{f:?}");
        }

        // Unreadable вместе с потерями: основная — драйвер (раньше по ADR-06), «+1».
        let lossy = path(
            AccessKind::Exclusive,
            DriverFormat::Unreadable { requested: fmt(44_100) },
            &[TransformKind::Resample { from: sr(44_100), to: sr(48_000), filter: SrcFilter::default() }],
        );
        assert_eq!(verdict(&lossy, GainState::Unity, None), excl_nbp(NbpReason::DriverParamsUnconfirmed, 1));

        // Confirmed(f ≠ requested) при иной причине: по §6.22 отдельной причины
        // для расхождения нет — показывается сама причина.
        let lossy = path(AccessKind::Exclusive, DriverFormat::Confirmed(fmt(48_000)), &[TransformKind::FloatNarrow]);
        assert_eq!(verdict(&lossy, GainState::Unity, None), excl_nbp(NbpReason::Truncation, 0));
    }

    /// §7.2, ТЗ-52: моно на стерео, underrun и несовпадение MD5 на бейдж не
    /// влияют. Underrun и MD5 живут в `DynamicPathState`, который `verdict` не
    /// принимает (сигнатура §2.5), поэтому достаточно проверить, что путь
    /// с моно-паддингом остаётся «Bit-perfect».
    #[test]
    fn badge_bitperfect_ignores_mono_underrun_md5() {
        use OutputSampleFormat::S16Le;

        let s = ModeSettings::default();
        let dev = caps_excl(&[44_100], &[S16Le]);
        let p = real_plan(&src_pcm(16, 44_100, 1), &dev, PathPolicy::Strict(&s.strict), GainState::NotApplicable);
        assert!(
            p.transforms.iter().any(|t| t.kind == TransformKind::MonoToStereo),
            "ожидался MonoToStereo: {:?}",
            p.transforms
        );
        let p = confirmed(p);
        // Состояние воспроизведения с ошибками: только для наглядности —
        // в вердикт оно не передаётся (ТЗ-52).
        let dynamics = DynamicPathState { underruns: 7, xruns: 3, md5: Md5Status::Mismatched, ..Default::default() };
        assert_eq!(dynamics.md5, Md5Status::Mismatched);
        assert_eq!(verdict(&p, GainState::NotApplicable, None), bit_perfect());

        // Остальные преобразования без причины (§6.22) тоже сохраняют «Bit-perfect».
        let lossless = [
            TransformKind::MonoToStereo,
            TransformKind::ChannelPad { from: 2, to: 4 },
            TransformKind::ZeroPad(ZeroPad::new(bd(16), bd(32)).unwrap()),
            TransformKind::IntToFloat { bits: bd(24) },
            TransformKind::DopPack { dsd: DsdRate::Dsd64 },
        ];
        assert_eq!(verdict(&exclusive(&lossless), GainState::Unity, None), bit_perfect());
    }

    /// §7.2, ТЗ-53: каждая `NbpReason` отдельно, включая `FloatToInt`, —
    /// минимальный путь даёт ровно эту основную причину без «+N».
    ///
    /// Из пути или `gain` достижимы все варианты, кроме `TestFailed`: он
    /// приходит только из записи «Теста» (`TestRecord`), поэтому строится
    /// через третий аргумент `verdict`. `Mute` в Exclusive из `gain` даёт
    /// отдельное состояние `ExclusiveBadge::Muted` (ОВС-18), поэтому причиной
    /// он проверяется в Shared и через преобразование `Mute` в пути.
    #[test]
    fn badge_each_reason() {
        let none = GainState::NotApplicable;
        let dsd_to_pcm = TransformKind::DsdToPcm {
            dsd: DsdRate::Dsd64,
            rate: sr(88_200),
            filter: DsdFilter::default(),
            stop_hz: 30_000,
            gain_comp: true,
        };
        let cases: Vec<(NbpReason, SignalPath, GainState)> = vec![
            (
                NbpReason::DriverParamsUnconfirmed,
                path(AccessKind::Exclusive, DriverFormat::Unreadable { requested: fmt(44_100) }, &[]),
                none,
            ),
            (NbpReason::DsdToPcm, one(dsd_to_pcm), none),
            (
                NbpReason::Resample,
                one(TransformKind::Resample { from: sr(44_100), to: sr(48_000), filter: SrcFilter::default() }),
                none,
            ),
            (
                NbpReason::Downmix,
                one(TransformKind::Downmix { from: layout_51(), to: ChannelLayout::stereo() }),
                none,
            ),
            (NbpReason::Truncation, one(TransformKind::Truncate(Truncate::new(bd(24), bd(16)).unwrap())), none),
            (NbpReason::Truncation, one(TransformKind::FloatNarrow), none),
            (NbpReason::FloatToInt, one(TransformKind::FloatToInt { bits: 24 }), none),
            (NbpReason::Int32ToFloat, one(TransformKind::Int32ToFloat), none),
            (NbpReason::Mute, one(TransformKind::Mute), GainState::Unity),
            (NbpReason::Volume, one(TransformKind::Volume(vol(50))), GainState::Unity),
            (NbpReason::Volume, exclusive(&[]), GainState::Scaled(vol(50))),
            (NbpReason::Dither, one(TransformKind::Dither(Dither::Tpdf)), none),
        ];
        for (reason, p, g) in &cases {
            assert_eq!(verdict(p, *g, None), excl_nbp(*reason, 0), "{reason:?}: {:?}", p.processing.transforms);
        }

        // Mute из gain — причина только в Shared.
        let shared = path(AccessKind::Shared, DriverFormat::Confirmed(fmt(44_100)), &[]);
        assert_eq!(
            verdict(&shared, GainState::Muted, None),
            BadgeState::Shared(SharedBadge::NotBitPerfect(nbp(NbpReason::Mute, 0)))
        );

        // TestFailed — из непройденной записи «Теста» с ключом пути.
        let failed = record(key(), false);
        assert_eq!(verdict(&exclusive(&[]), none, Some(&failed)), excl_nbp(NbpReason::TestFailed, 0));

        // Покрыты все варианты NbpReason.
        let mut seen: Vec<NbpReason> = cases.iter().map(|(r, _, _)| *r).collect();
        seen.push(NbpReason::TestFailed);
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen, ALL_REASONS.to_vec());
    }

    /// Все варианты `NbpReason` в порядке приоритета ADR-06 (§2.5).
    const ALL_REASONS: [NbpReason; 11] = [
        NbpReason::TestFailed,
        NbpReason::DriverParamsUnconfirmed,
        NbpReason::DsdToPcm,
        NbpReason::Resample,
        NbpReason::Downmix,
        NbpReason::Truncation,
        NbpReason::FloatToInt,
        NbpReason::Int32ToFloat,
        NbpReason::Mute,
        NbpReason::Volume,
        NbpReason::Dither,
    ];

    /// §7.2, ТЗ-53, ADR-06: SRC + downmix → основная «ресемплинг», `more = 1`;
    /// порядок причин — `Ord` `NbpReason`, независимо от порядка в пути.
    #[test]
    fn badge_reason_priority_and_more() {
        // Порядок вариантов строго возрастает (ADR-06).
        assert!(ALL_REASONS.windows(2).all(|w| w[0] < w[1]), "{ALL_REASONS:?}");

        let resample = TransformKind::Resample { from: sr(44_100), to: sr(48_000), filter: SrcFilter::default() };
        let downmix = TransformKind::Downmix { from: layout_51(), to: ChannelLayout::stereo() };
        // Порядок в пути не важен.
        for kinds in [[resample.clone(), downmix.clone()], [downmix.clone(), resample.clone()]] {
            assert_eq!(
                verdict(&exclusive(&kinds), GainState::NotApplicable, None),
                excl_nbp(NbpReason::Resample, 1),
                "{kinds:?}"
            );
        }

        // Реальный план: 5.1 44,1 кГц на стерео 48 кГц — SRC + downmix (+ TPDF).
        let s = ModeSettings::default();
        let optimal = PathPolicy::Optimal(&s.optimal);
        let dev = caps_excl(&[48_000], &[OutputSampleFormat::S24Le]);
        let p = confirmed(real_plan(&src_pcm(24, 44_100, 6), &dev, optimal, GainState::Unity));
        assert_eq!(verdict(&p, GainState::Unity, None), excl_nbp(NbpReason::Resample, 2), "{:?}", p.processing);

        // Набор всех причин в обратном приоритету порядке.
        let failed = record(key(), false);
        let all = [
            TransformKind::Dither(Dither::Tpdf),
            TransformKind::Volume(vol(50)),
            TransformKind::Mute,
            TransformKind::Int32ToFloat,
            TransformKind::FloatToInt { bits: 24 },
            TransformKind::FloatNarrow,
            downmix,
            resample,
        ];
        let unreadable = DriverFormat::Unreadable { requested: fmt(44_100) };
        // Все причины сразу: TestFailed основная, «+10».
        let p = path(AccessKind::Exclusive, unreadable, &all);
        let mut with_dsd: Vec<TransformKind> = all.to_vec();
        with_dsd.push(TransformKind::DsdToPcm {
            dsd: DsdRate::Dsd64,
            rate: sr(88_200),
            filter: DsdFilter::default(),
            stop_hz: 30_000,
            gain_comp: true,
        });
        let p_all = path(AccessKind::Exclusive, unreadable, &with_dsd);
        assert_eq!(
            verdict(&p_all, GainState::Scaled(vol(50)), Some(&failed)),
            excl_nbp(NbpReason::TestFailed, 10)
        );
        // Без теста — DriverParamsUnconfirmed, без DSD→PCM: 8 остальных
        // (Resample, Downmix, Truncation, FloatToInt, Int32ToFloat, Mute, Volume, Dither).
        assert_eq!(verdict(&p, GainState::Unity, None), excl_nbp(NbpReason::DriverParamsUnconfirmed, 8));
    }

    // -----------------------------------------------------------------------
    // Тесты бейджа §7.2 (ТЗ-54, ТЗ-56, ТЗ-57, ТЗ-58), часть 2
    // -----------------------------------------------------------------------

    /// Shared-устройство (звуковой сервер) стерео.
    fn caps_shared(hz: &[u32], formats: &[OutputSampleFormat]) -> DeviceCaps {
        caps(AccessKind::Shared, OutputApi::WasapiShared, hz, formats)
    }

    /// Бейдж текущего потока: без снимка пути (поток не открыт или последнее
    /// открытие не удалось) — `NoStream`, иначе `verdict` (ТЗ-56, §2.5).
    /// Модельная часть правила, которое выполняет `UiAudioState`.
    fn badge_of(current: Option<&SignalPath>, gain: GainState, test: Option<&TestRecord>) -> BadgeState {
        current.map_or(BadgeState::NoStream, |p| verdict(p, gain, test))
    }

    /// Ключ «Теста», совпадающий с путём `p` (`ConfigKey::matches_path`).
    fn key_of(p: &SignalPath) -> ConfigKey {
        let req = &p.output.requested;
        ConfigKey {
            device: p.output.device_name.clone(),
            mode: p.mode,
            rate: req.rate,
            format: req.sample,
            channels: req.channels.get(),
            player_version: "0.1.0".to_owned(),
            kernel: "6.8.0".to_owned(),
        }
    }

    /// §7.2, ТЗ-54, И-Т9: в Совместимом режиме бейдж всегда `Shared(..)` —
    /// при любых источниках, возможностях сервера, ответе драйвера и усилении,
    /// в том числе при идеальном passthrough; `Exclusive(BitPerfect)`
    /// непредставим. 24/96 при 100 % → «Shared: без изменений»; 80 % → «громкость».
    #[test]
    fn compatible_badge_never_bitperfect() {
        use OutputSampleFormat::{S16Le, S24Le, S32Le, F32};

        let s = ModeSettings::default();
        let compatible = PathPolicy::Compatible(&s.compatible);

        // Строка спеки: 24/96 при 100 % и при 80 %.
        let server = caps_shared(&[96_000], &[F32]);
        let p = real_plan(&src_pcm(24, 96_000, 2), &server, compatible, GainState::Unity);
        let f = p.target;
        let p = opened(p, AccessKind::Shared, DriverFormat::Confirmed(f));
        assert_eq!(verdict(&p, GainState::Unity, None), BadgeState::Shared(SharedBadge::Clean));
        let v80 = GainState::Scaled(vol(80));
        let p = real_plan(&src_pcm(24, 96_000, 2), &server, compatible, v80);
        let f = p.target;
        let p = opened(p, AccessKind::Shared, DriverFormat::Confirmed(f));
        assert!(
            matches!(
                verdict(&p, v80, None),
                BadgeState::Shared(SharedBadge::NotBitPerfect(NbpSummary { primary: NbpReason::Volume, .. }))
            ),
            "80 %: {:?}",
            p.processing.transforms
        );

        // Перебор: источники × сервер × драйвер × усиление.
        let sources = [
            src_pcm(16, 44_100, 2),
            src_pcm(24, 96_000, 2),
            src_pcm(24, 192_000, 2),
            src_pcm(16, 44_100, 1),
            src_pcm(24, 48_000, 6),
            src_dsd(DsdRate::Dsd64),
        ];
        let servers = [
            caps_shared(&[44_100], &[S16Le]),
            caps_shared(&[48_000], &[S32Le]),
            caps_shared(&[96_000], &[F32]),
            caps_shared(&[44_100, 48_000, 96_000, 192_000], &[S16Le, S24Le, S32Le, F32]),
        ];
        let gains = [GainState::NotApplicable, GainState::Unity, v80, GainState::Muted];
        let mut clean = 0usize;
        for src in &sources {
            for server in &servers {
                for g in gains {
                    let p = real_plan(src, server, compatible, g);
                    assert_eq!(p.mode, ModeKind::Compatible);
                    let f = p.target;
                    let drivers = [DriverFormat::Confirmed(f), DriverFormat::Unreadable { requested: f }];
                    for d in drivers {
                        let path = opened(p.clone(), AccessKind::Shared, d);
                        let b = verdict(&path, g, None);
                        assert!(matches!(b, BadgeState::Shared(_)), "{src:?} {server:?} {g:?} {d:?}: {b:?}");
                        assert_ne!(b, bit_perfect());
                        if b == BadgeState::Shared(SharedBadge::Clean) {
                            clean += 1;
                        }
                    }
                }
            }
        }
        // Перебор содержит и чистые случаи (passthrough до сервера).
        assert!(clean > 0);

        // Идеальный passthrough литералом: без преобразований, Confirmed ==
        // запрошенному, без регулировки — всё равно Shared(Clean).
        let ideal = path(AccessKind::Shared, DriverFormat::Confirmed(fmt(44_100)), &[]);
        for g in [GainState::NotApplicable, GainState::Unity] {
            assert_eq!(verdict(&ideal, g, None), BadgeState::Shared(SharedBadge::Clean));
            // Пройденный «Тест» с совпадающим ключом Bit-perfect не даёт.
            let passed = record(key_of(&ideal), true);
            assert_eq!(verdict(&ideal, g, Some(&passed)), BadgeState::Shared(SharedBadge::Clean));
        }
    }

    /// §7.2, ТЗ-56, И-Р14: A открыт → открытие B не удалось → `NoStream`,
    /// отчёт «поток не открыт» без параметров A.
    ///
    /// `verdict` всегда получает снимок пути, поэтому здесь проверяется
    /// модельная часть: план B заблокирован до открытия, снимка B нет, снимок A
    /// не переиспользуется → `NoStream`; «Тест» недоступен с `NoStream`.
    /// Хранение снимка и сброс бейджа в UI (`UiAudioState`) — шаг 26.
    #[test]
    fn badge_no_stream_after_failed_open() {
        use crate::audio::error::Incompatibility;
        use crate::audio::path::StageId;
        use crate::audio::test_run::{test_availability, TestAvailability, TestBlocker};
        use OutputSampleFormat::{S24Le, S32Le};

        let s = ModeSettings::default();
        let strict = PathPolicy::Strict(&s.strict);
        let na = GainState::NotApplicable;
        // Без 176,4 кГц: DoP для DSD64 невозможен.
        let dev = caps_excl(&[44_100, 88_200, 192_000], &[S24Le, S32Le]);

        // Трек A: 24/192 открыт, «Bit-perfect».
        let a = confirmed(real_plan(&src_pcm(24, 192_000, 2), &dev, strict, na));
        let failed_a = record(key_of(&a), false);
        let mut current: Option<SignalPath> = Some(a);
        assert_eq!(badge_of(current.as_ref(), na, None), bit_perfect());

        // Трек B: DSD64 в Строгом без DoP → Blocked на этапе «Вывод», без открытия.
        let b = src_dsd(DsdRate::Dsd64);
        let blocked = match plan(&b, &dev, strict) {
            Ok(p) => panic!("ожидалась блокировка B: {p:?}"),
            Err(bl) => bl,
        };
        assert!(matches!(blocked.at, StageId::Output), "{:?}", blocked.at);
        assert!(
            matches!(blocked.reason, Incompatibility::DopUnsupported { dsd: DsdRate::Dsd64, .. }),
            "{:?}",
            blocked.reason
        );
        // Отчёт строится из частичного плана B: источник — B, не A.
        assert_eq!(blocked.partial.source, b);
        assert!(matches!(blocked.partial.source.kind, SourceKind::Dsd { .. }));

        // Неудачное открытие снимает снимок A: бейдж «нет потока».
        current = None;
        assert_eq!(badge_of(current.as_ref(), na, None), BadgeState::NoStream);
        assert_eq!(badge_of(current.as_ref(), GainState::Muted, None), BadgeState::NoStream);
        // Запись «Теста» для конфигурации A на бейдж без потока не влияет.
        assert_eq!(badge_of(current.as_ref(), na, Some(&failed_a)), BadgeState::NoStream);
        assert_eq!(
            test_availability(&BadgeState::NoStream, ModeKind::Strict),
            TestAvailability::Disabled(TestBlocker::NoStream)
        );
    }

    /// §7.2, ТЗ-57: бейдж DSD выводится из фактического пути, а не из настроек
    /// (замена `dsd_pcm_breaks_bit_perfect_detects_conflict`). При тех же
    /// настройках Оптимального режима (DoP разрешён) устройство с DoP даёт
    /// «Bit-perfect», устройство без DoP — «DSD→PCM».
    #[test]
    fn badge_from_path_not_settings_dsd() {
        use OutputSampleFormat::{S24Le, S32Le};

        let s = ModeSettings::default();
        let optimal = PathPolicy::Optimal(&s.optimal);
        let na = GainState::NotApplicable;
        let unity = GainState::Unity;
        let src = src_dsd(DsdRate::Dsd64);

        // Устройство с 176,4 кГц: DoP DSD64 → «Bit-perfect».
        let dev_dop = caps_excl(&[176_400, 352_800], &[S24Le, S32Le]);
        let p = confirmed(real_plan(&src, &dev_dop, optimal, na));
        assert!(matches!(p.route, Route::Dop(_)), "ожидался DoP: {:?}", p.route);
        assert_eq!(verdict(&p, na, None), bit_perfect());

        // Те же настройки, устройство без DoP: путь DSD→PCM → «DSD→PCM» (+ TPDF).
        let dev_pcm = caps_excl(&[44_100, 88_200], &[S24Le, S32Le]);
        let p = confirmed(real_plan(&src, &dev_pcm, optimal, unity));
        assert!(matches!(p.route, Route::DsdToPcm { .. }), "ожидался DSD→PCM: {:?}", p.route);
        assert!(p.dsd_chain.is_some());
        assert_eq!(verdict(&p, unity, None), excl_nbp(NbpReason::DsdToPcm, 1));

        // Режим пути (настройка) причину не подавляет: DsdToPcm в пути
        // Строгого режима — всё равно «DSD→PCM»; DopPack в пути — не причина.
        let dsd_to_pcm = TransformKind::DsdToPcm {
            dsd: DsdRate::Dsd64,
            rate: sr(88_200),
            filter: DsdFilter::default(),
            stop_hz: 30_000,
            gain_comp: true,
        };
        let mut strict_plan = plan_with(&[dsd_to_pcm]);
        strict_plan.mode = ModeKind::Strict;
        assert_eq!(verdict(&confirmed(strict_plan), na, None), excl_nbp(NbpReason::DsdToPcm, 0));
        let mut dop_plan = plan_with(&[TransformKind::DopPack { dsd: DsdRate::Dsd64 }]);
        dop_plan.mode = ModeKind::Optimal;
        assert_eq!(verdict(&confirmed(dop_plan), na, None), bit_perfect());

        // Совместимый: DSD всегда через PCM → Shared с причиной «DSD→PCM».
        let compatible = PathPolicy::Compatible(&s.compatible);
        let server = caps_shared(&[88_200, 176_400], &[OutputSampleFormat::F32]);
        let p = real_plan(&src, &server, compatible, unity);
        let f = p.target;
        let p = opened(p, AccessKind::Shared, DriverFormat::Confirmed(f));
        assert!(
            matches!(
                verdict(&p, unity, None),
                BadgeState::Shared(SharedBadge::NotBitPerfect(NbpSummary { primary: NbpReason::DsdToPcm, .. }))
            ),
            "{:?}",
            p.processing.transforms
        );
    }

    /// §7.2, ТЗ-58: непройденный «Тест» (✕) с ключом, совпадающим с путём, →
    /// «тест не пройден»; при отличии устройства, режима, частоты, формата или
    /// каналов, а также при пройденном тесте — обычные правила §6.22.
    #[test]
    fn badge_test_failed_matching_config() {
        use OutputSampleFormat::{S24Le, S32Le};

        let s = ModeSettings::default();
        let strict = PathPolicy::Strict(&s.strict);
        let na = GainState::NotApplicable;
        let dev = caps_excl(&[96_000, 192_000], &[S24Le, S32Le]);
        let p = confirmed(real_plan(&src_pcm(24, 192_000, 2), &dev, strict, na));
        let k = key_of(&p);
        assert!(k.matches_path(&p));

        // ✕ для ключа пути → «тест не пройден», единственная причина.
        assert_eq!(verdict(&p, na, Some(&record(k.clone(), false))), excl_nbp(NbpReason::TestFailed, 0));
        // ✓ для того же ключа → обычные правила («Bit-perfect»).
        assert_eq!(verdict(&p, na, Some(&record(k.clone(), true))), bit_perfect());

        // Другой ключ → обычные правила.
        let others = [
            ("device", ConfigKey { device: "hw:1".to_owned(), ..k.clone() }),
            ("mode", ConfigKey { mode: ModeKind::Optimal, ..k.clone() }),
            ("rate", ConfigKey { rate: sr(96_000), ..k.clone() }),
            ("format", ConfigKey { format: S32Le, ..k.clone() }),
            ("channels", ConfigKey { channels: 4, ..k.clone() }),
        ];
        for (name, other) in &others {
            assert!(!other.matches_path(&p), "{name}");
            assert_eq!(verdict(&p, na, Some(&record(other.clone(), false))), bit_perfect(), "{name}");
        }

        // Путь с потерями: ✕ совпадающего ключа — основная причина, «+N»;
        // другой ключ — обычная причина пути.
        let opt = PathPolicy::Optimal(&s.optimal);
        let v80 = GainState::Scaled(vol(80));
        let dev_16 = caps_excl(&[44_100], &[OutputSampleFormat::S16Le]);
        let lossy = confirmed(real_plan(&src_pcm(16, 44_100, 2), &dev_16, opt, v80));
        let lk = key_of(&lossy);
        assert_eq!(verdict(&lossy, v80, Some(&record(lk.clone(), false))), excl_nbp(NbpReason::TestFailed, 2));
        assert_eq!(verdict(&lossy, v80, Some(&record(lk.clone(), true))), excl_nbp(NbpReason::Volume, 1));
        let other = ConfigKey { rate: sr(48_000), ..lk };
        assert_eq!(verdict(&lossy, v80, Some(&record(other, false))), excl_nbp(NbpReason::Volume, 1));
    }
}
