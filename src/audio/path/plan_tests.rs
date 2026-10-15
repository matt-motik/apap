//! Тесты планировщика `plan()` по таблице §7.2 (AM1.0 §6.4, §6.5, §6.13,
//! ТЗ-22, ТЗ-27, ТЗ-36, ТЗ-54, ТЗ-133, ОВ-3, ОВ-4, ОВС-2): выбор частоты и
//! правила каналов в трёх режимах на синтетических `DeviceCaps` — без
//! реального устройства (ТЗ-114).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use smallvec::SmallVec;

use crate::audio::backend::{AccessKind, DeviceCaps, OutputApi, RateSet};
use crate::audio::error::Incompatibility;
use crate::audio::format::{
    BitDepth, ChannelLayout, ChannelPos, Codec, Container, DsdRate, OutputSampleFormat, SampleRate, SourceFormat,
    SourceKind,
};
use crate::settings::params::ParamId;
use crate::settings::playback::{Dither, DsdFilter, ModeKind, ModeSettings, PathPolicy, RateFallbackRule, Volume};

use crate::audio::session::RingPayload;
use crate::audio::verdict::{GainState, NbpReason};

use super::super::transform::{Cause, Lossiness, Transform, TransformKind, Truncate, ZeroPad};
use super::super::{Blocked, DsdChainStep, DsdStepKind, DsdStepStatus, PathPlan, Route, StageId};
use super::{dsd_pcm_rate, dsd_stop_hz, plan, requantizes, with_gain};

fn sr(hz: u32) -> SampleRate {
    SampleRate::new(hz).unwrap()
}

fn bd(bits: u8) -> BitDepth {
    BitDepth::new(bits).unwrap()
}

fn rates(hz: &[u32]) -> RateSet {
    RateSet::Discrete(hz.iter().map(|&h| sr(h)).collect())
}

/// PCM-источник FLAC: `bits` бит, `rate` Гц, `ch` каналов.
fn src_pcm(bits: u8, rate: u32, ch: u8) -> SourceFormat {
    let layout = match ch {
        1 => ChannelLayout::mono(),
        2 => ChannelLayout::stereo(),
        n => {
            let positions: Vec<ChannelPos> = (0..n).map(ChannelPos::Unknown).collect();
            ChannelLayout::new(&positions).unwrap()
        }
    };
    SourceFormat {
        container: Container::Flac,
        codec: Codec::Flac,
        lossy: false,
        rate: sr(rate),
        layout,
        kind: SourceKind::Pcm { bits: BitDepth::new(bits).unwrap() },
    }
}

fn caps(access: AccessKind, api: OutputApi, rates: RateSet, formats: &[OutputSampleFormat]) -> DeviceCaps {
    DeviceCaps {
        api,
        access,
        name: "test".to_owned(),
        rates,
        formats: SmallVec::from_slice(formats),
        channels: SmallVec::from_slice(&[2]),
    }
}

/// Shared-сервер (Совместимый режим).
fn caps_shared(rates: RateSet, formats: &[OutputSampleFormat]) -> DeviceCaps {
    caps(AccessKind::Shared, OutputApi::WasapiShared, rates, formats)
}

/// Exclusive-устройство (Оптимальный и Строгий режимы).
fn caps_exclusive(rates: RateSet, formats: &[OutputSampleFormat]) -> DeviceCaps {
    caps(AccessKind::Exclusive, OutputApi::AlsaHw, rates, formats)
}

/// Источник 5.1 (FLAC 24/48) с раскладкой ITU: FL FR FC LFE BL BR (§6.13).
fn src_51() -> SourceFormat {
    use ChannelPos::{BL, BR, FC, FL, FR, LFE};
    let mut s = src_pcm(24, 48_000, 6);
    s.layout = ChannelLayout::new(&[FL, FR, FC, LFE, BL, BR]).unwrap();
    s
}

/// `DeviceCaps` с заданным набором чисел каналов `C` (§6.5).
fn with_channels(mut c: DeviceCaps, channels: &[u8]) -> DeviceCaps {
    c.channels = SmallVec::from_slice(channels);
    c
}

/// Политика каждого из трёх режимов (ADR-04).
fn all_policies(s: &ModeSettings) -> [PathPolicy<'_>; 3] {
    [PathPolicy::Compatible(&s.compatible), PathPolicy::Optimal(&s.optimal), PathPolicy::Strict(&s.strict)]
}

/// Преобразования каналов плана (§6.5) по порядку.
fn channel_kinds(p: &PathPlan) -> Vec<&TransformKind> {
    p.transforms
        .iter()
        .map(|t| &t.kind)
        .filter(|k| {
            matches!(
                k,
                TransformKind::MonoToStereo | TransformKind::ChannelPad { .. } | TransformKind::Downmix { .. }
            )
        })
        .collect()
}

fn settings() -> ModeSettings {
    ModeSettings::default()
}

fn resamples(p: &PathPlan) -> Vec<&Transform> {
    p.transforms
        .iter()
        .filter(|t| matches!(t.kind, TransformKind::Resample { .. }))
        .collect()
}

/// Единственный `Resample` плана: (from, to, cause).
fn single_resample(p: &PathPlan) -> (SampleRate, SampleRate, Cause) {
    let rs = resamples(p);
    assert_eq!(rs.len(), 1, "ожидался ровно один Resample: {:?}", p.transforms);
    match rs[0].kind {
        TransformKind::Resample { from, to, .. } => (from, to, rs[0].cause.clone()),
        _ => unreachable!(),
    }
}

/// §7.2, ТЗ-22: 24/96 стерео, сервер принимает 96/2 — без SRC.
#[test]
fn compatible_passthrough_when_server_accepts() {
    let s = settings();
    let c = caps_shared(rates(&[48_000, 96_000]), &[OutputSampleFormat::F32]);
    let p = plan(&src_pcm(24, 96_000, 2), &c, PathPolicy::Compatible(&s.compatible)).unwrap();
    assert!(resamples(&p).is_empty(), "лишний Resample: {:?}", p.transforms);
    assert_eq!(p.target.rate, sr(96_000));
    assert_eq!(p.target.channels.get(), 2);
    // Формат сэмпла (§6.5, Shared): выход `F32`, единственное преобразование —
    // `IntToFloat { 24 }` без потерь с причиной `ServerLimit`.
    assert_eq!(p.target.sample, OutputSampleFormat::F32);
    let kinds: Vec<&TransformKind> = p.transforms.iter().map(|t| &t.kind).collect();
    assert_eq!(kinds, vec![&TransformKind::IntToFloat { bits: bd(24) }], "ожидался только IntToFloat");
    assert_eq!(p.transforms[0].cause, Cause::ServerLimit);
    assert_eq!(p.transforms[0].kind.lossiness(), Lossiness::Lossless);
}

/// §7.2, ТЗ-22: сервер принимает только 48 кГц — `Resample 96→48`, `ServerLimit`.
#[test]
fn compatible_src_when_server_limits() {
    let s = settings();
    let c = caps_shared(rates(&[48_000]), &[OutputSampleFormat::F32]);
    let p = plan(&src_pcm(24, 96_000, 2), &c, PathPolicy::Compatible(&s.compatible)).unwrap();
    assert_eq!(single_resample(&p), (sr(96_000), sr(48_000), Cause::ServerLimit));
    assert_eq!(p.target.rate, sr(48_000));
}

/// §7.2, ТЗ-22, ОВ-4: фиксированная частота 48 кГц при поддержке 96 и 48 —
/// `Resample`, `PlayerSetting(FixedRate)`.
#[test]
fn compatible_fixed_rate_src() {
    let mut s = settings();
    s.compatible.fixed_rate = Some(sr(48_000));
    let c = caps_shared(rates(&[96_000, 48_000]), &[OutputSampleFormat::F32]);
    let p = plan(&src_pcm(24, 96_000, 2), &c, PathPolicy::Compatible(&s.compatible)).unwrap();
    assert_eq!(
        single_resample(&p),
        (sr(96_000), sr(48_000), Cause::PlayerSetting(ParamId::FixedRate))
    );
    assert_eq!(p.target.rate, sr(48_000));
}

/// §7.2, ТЗ-27, ОВ-3: все случаи критерия ТЗ-27 для трёх правил; по
/// умолчанию — `SameFamily`.
#[test]
fn optimal_rate_fallback_rules() {
    use RateFallbackRule::{Nearest, NoDownsample, SameFamily};

    assert_eq!(settings().optimal.rate_fallback, SameFamily);

    const ALL: [RateFallbackRule; 3] = [SameFamily, Nearest, NoDownsample];
    let dev4: &[u32] = &[44_100, 48_000, 88_200, 96_000];
    let dev48_96: &[u32] = &[48_000, 96_000];
    let dev441_96: &[u32] = &[44_100, 96_000];

    // (правило, кандидаты, источник, ожидаемая частота).
    let mut cases: Vec<(RateFallbackRule, &[u32], u32, u32)> = Vec::new();
    // ТЗ-27: {44.1, 48, 88.2, 96}, трек 192 → 96 при любом правиле
    // (для NoDownsample — нет частот ≥ f → max).
    for rule in ALL {
        cases.push((rule, dev4, 192_000, 96_000));
    }
    // ТЗ-27: {48, 96}, трек 44.1 → 48 при любом правиле
    // (SameFamily: частот семейства 44.1 нет → Nearest).
    for rule in ALL {
        cases.push((rule, dev48_96, 44_100, 48_000));
    }
    // ТЗ-27: {44.1, 96}, трек 88.2.
    cases.push((SameFamily, dev441_96, 88_200, 44_100));
    cases.push((Nearest, dev441_96, 88_200, 96_000));
    cases.push((NoDownsample, dev441_96, 88_200, 96_000));
    // SameFamily: нет частоты семейства ≤ f → минимальная того же семейства выше.
    cases.push((SameFamily, &[48_000, 88_200, 176_400], 44_100, 88_200));
    // Nearest: при равном расстоянии — большая.
    cases.push((Nearest, &[40_000, 48_000], 44_000, 48_000));
    // NoDownsample: минимальная не ниже исходной.
    cases.push((NoDownsample, &[44_100, 96_000, 192_000], 88_200, 96_000));

    for (rule, cands, src_hz, want_hz) in cases {
        let mut s = settings();
        s.optimal.rate_fallback = rule;
        let c = caps_exclusive(rates(cands), &[OutputSampleFormat::S32Le]);
        let p = plan(&src_pcm(24, src_hz, 2), &c, PathPolicy::Optimal(&s.optimal))
            .unwrap_or_else(|b| panic!("{rule:?} {cands:?} {src_hz}: Blocked {b:?}"));
        assert_eq!(
            single_resample(&p),
            (sr(src_hz), sr(want_hz), Cause::DeviceLimit),
            "{rule:?} {cands:?} {src_hz}"
        );
        assert_eq!(p.target.rate, sr(want_hz), "{rule:?} {cands:?} {src_hz}");
    }

    // Частота поддерживается — без SRC при любом правиле.
    for rule in ALL {
        let mut s = settings();
        s.optimal.rate_fallback = rule;
        let c = caps_exclusive(rates(dev441_96), &[OutputSampleFormat::S32Le]);
        let p = plan(&src_pcm(24, 96_000, 2), &c, PathPolicy::Optimal(&s.optimal)).unwrap();
        assert!(resamples(&p).is_empty(), "{rule:?}: лишний Resample {:?}", p.transforms);
        assert_eq!(p.target.rate, sr(96_000));
    }
}

/// §7.2, ТЗ-36: Exclusive-устройство без 192 кГц, трек 24/192 —
/// `Blocked { at: Output, RateUnsupported }`.
#[test]
fn strict_rejects_rate() {
    let s = settings();
    let set = rates(&[44_100, 48_000, 88_200, 96_000]);
    let c = caps_exclusive(set.clone(), &[OutputSampleFormat::S24Le]);
    let err = plan(&src_pcm(24, 192_000, 2), &c, PathPolicy::Strict(&s.strict)).unwrap_err();
    let Blocked { at, reason, .. } = err;
    assert_eq!(at, StageId::Output);
    assert_eq!(reason, Incompatibility::RateUnsupported { rate: sr(192_000), supported: set });
    // Текст «частота 192 кГц не поддерживается устройством» не проверяется:
    // у `Incompatibility` пока нет `Display`/текстового метода.
}

/// §7.2, ТЗ-22, ТЗ-12: 5.1 на сервере с 2 каналами — `Downmix 5.1 → 2.0`,
/// `ServerLimit`, цель — 2 канала.
#[test]
fn compatible_downmix_when_server_limits() {
    let s = settings();
    let c = caps_shared(rates(&[48_000]), &[OutputSampleFormat::F32]);
    let p = plan(&src_51(), &c, PathPolicy::Compatible(&s.compatible)).unwrap();
    let downmixes: Vec<&Transform> = p
        .transforms
        .iter()
        .filter(|t| matches!(t.kind, TransformKind::Downmix { .. }))
        .collect();
    assert_eq!(downmixes.len(), 1, "ожидался один Downmix: {:?}", p.transforms);
    assert_eq!(downmixes[0].cause, Cause::ServerLimit);
    assert_eq!(p.target.channels.get(), 2);
}

/// §7.2, ТЗ-29: Оптимальный, 5.1 на стерео-устройстве — `Downmix 5.1 → 2.0`
/// с раскладками ITU, причина бейджа — `Downmix`.
#[test]
fn optimal_downmix_51_to_stereo_uses_itu_matrix() {
    let s = settings();
    let c = caps_exclusive(rates(&[48_000]), &[OutputSampleFormat::S32Le]);
    let p = plan(&src_51(), &c, PathPolicy::Optimal(&s.optimal)).unwrap();
    // План несёт только раскладки: матрица ITU `G[out][in]` (§6.13) строится
    // по ним в потоке декодирования, поэтому здесь проверяются from/to.
    let kinds = channel_kinds(&p);
    assert_eq!(kinds.len(), 1, "ожидался один Downmix: {:?}", p.transforms);
    let TransformKind::Downmix { from, to } = kinds[0] else {
        panic!("ожидался Downmix: {:?}", kinds[0]);
    };
    assert_eq!(*from, src_51().layout);
    assert_eq!(*to, ChannelLayout::stereo());
    assert_eq!(kinds[0].badge_reason(), Some(NbpReason::Downmix));
    assert_eq!(p.target.channels.get(), 2);
    let t = p.transforms.iter().find(|t| matches!(t.kind, TransformKind::Downmix { .. })).unwrap();
    assert_eq!(t.cause, Cause::DeviceLimit);
}

/// §7.2, ТЗ-36: Строгий, 5.1 на стерео — `Blocked { at: Output,
/// ChannelsUnsupported { src: 6 } }`.
#[test]
fn strict_rejects_channel_mismatch() {
    let s = settings();
    let c = caps_exclusive(rates(&[48_000]), &[OutputSampleFormat::S24Le]);
    let err = plan(&src_51(), &c, PathPolicy::Strict(&s.strict)).unwrap_err();
    let Blocked { at, reason, .. } = err;
    assert_eq!(at, StageId::Output);
    assert_eq!(reason, Incompatibility::ChannelsUnsupported { src: 6, supported: SmallVec::from_slice(&[2]) });
    // Текст «число каналов 6 не поддерживается устройством» проверяется в
    // шаге отчёта (AM1.0-8.5 шаг 22): у `Incompatibility` пока нет `Display`.
}

/// §7.2, ТЗ-13, ТЗ-36: Строгий, моно на стерео — `MonoToStereo`, не
/// несовместимость.
#[test]
fn strict_mono_on_stereo_plays() {
    let s = settings();
    let c = caps_exclusive(rates(&[44_100]), &[OutputSampleFormat::S16Le]);
    let p = plan(&src_pcm(16, 44_100, 1), &c, PathPolicy::Strict(&s.strict)).unwrap();
    assert_eq!(channel_kinds(&p), vec![&TransformKind::MonoToStereo]);
    assert_eq!(p.target.channels.get(), 2);
}

/// §7.2, ТЗ-133, ОВС-2: стерео на {4, 8} → `ChannelPad 2→4` без потерь;
/// моно на {4} → `MonoToStereo` + `ChannelPad 2→4`; во всех режимах, в
/// Строгом — не `Blocked`. Нули в каналах 3–4 записывает поток
/// декодирования (§6.5), план фиксирует только размещение.
#[test]
fn channel_pad_to_wider_device_all_modes() {
    let s = settings();
    for policy in all_policies(&s) {
        let c = with_channels(caps_exclusive(rates(&[48_000]), &[OutputSampleFormat::S24Le]), &[4, 8]);
        let p = plan(&src_pcm(24, 48_000, 2), &c, policy)
            .unwrap_or_else(|b| panic!("{policy:?}: стерео на {{4, 8}} Blocked {b:?}"));
        assert_eq!(channel_kinds(&p), vec![&TransformKind::ChannelPad { from: 2, to: 4 }], "{policy:?}");
        assert!(
            p.transforms
                .iter()
                .filter(|t| matches!(t.kind, TransformKind::ChannelPad { .. }))
                .all(|t| t.kind.lossiness() == Lossiness::Lossless),
            "{policy:?}: ChannelPad с потерями"
        );
        assert_eq!(p.target.channels.get(), 4, "{policy:?}");

        let c = with_channels(caps_exclusive(rates(&[48_000]), &[OutputSampleFormat::S24Le]), &[4]);
        let p = plan(&src_pcm(24, 48_000, 1), &c, policy)
            .unwrap_or_else(|b| panic!("{policy:?}: моно на {{4}} Blocked {b:?}"));
        assert_eq!(
            channel_kinds(&p),
            vec![&TransformKind::MonoToStereo, &TransformKind::ChannelPad { from: 2, to: 4 }],
            "{policy:?}"
        );
        assert!(
            channel_kinds(&p).iter().all(|k| k.lossiness() == Lossiness::Lossless),
            "{policy:?}: преобразование каналов с потерями"
        );
        assert_eq!(p.target.channels.get(), 4, "{policy:?}");
    }
}

/// Виды преобразований плана по порядку.
fn kinds(p: &PathPlan) -> Vec<&TransformKind> {
    p.transforms.iter().map(|t| &t.kind).collect()
}

/// Есть ли `Dither(Tpdf)` в плане после `with_gain` с единичным усилением
/// (§6.3 шаг 6, ТЗ-8).
fn gets_tpdf(p: &PathPlan, policy: PathPolicy<'_>) -> bool {
    let g = with_gain(p, GainState::Unity, policy);
    g.transforms.iter().any(|t| t.kind == TransformKind::Dither(Dither::Tpdf))
}

/// §7.2, ТЗ-26: Оптимальный, 24/192 стерео на устройстве с 192 кГц и
/// `S24_LE`/`S32_LE` — passthrough: ни одного преобразования (24 бита в
/// `S24_LE` — без `ZeroPad`, ОВ-32), переквантования нет.
#[test]
fn optimal_passthrough_24_192() {
    let s = settings();
    let c = caps_exclusive(
        rates(&[44_100, 96_000, 192_000]),
        &[OutputSampleFormat::S32Le, OutputSampleFormat::S24Le, OutputSampleFormat::S16Le],
    );
    let p = plan(&src_pcm(24, 192_000, 2), &c, PathPolicy::Optimal(&s.optimal)).unwrap();
    assert!(
        p.transforms.iter().all(|t| t.kind.lossiness() == Lossiness::Lossless),
        "преобразование с потерями: {:?}",
        p.transforms
    );
    assert!(p.transforms.is_empty(), "лишние преобразования: {:?}", p.transforms);
    assert_eq!(p.target.rate, sr(192_000));
    assert_eq!(p.target.sample, OutputSampleFormat::S24Le);
    assert_eq!(p.target.channels.get(), 2);
    assert!(!requantizes(&p));
}

/// §7.2, ТЗ-26: Оптимальный, 16-битный трек на устройстве только с
/// `S32_LE` — ровно один `ZeroPad 16→32` без потерь (`FormatContainer`).
#[test]
fn optimal_zero_pad_16_to_32() {
    let s = settings();
    let c = caps_exclusive(rates(&[44_100]), &[OutputSampleFormat::S32Le]);
    let p = plan(&src_pcm(16, 44_100, 2), &c, PathPolicy::Optimal(&s.optimal)).unwrap();
    let pad = TransformKind::ZeroPad(ZeroPad::new(bd(16), bd(32)).unwrap());
    assert_eq!(kinds(&p), vec![&pad], "ожидался один ZeroPad 16→32");
    assert_eq!(p.transforms[0].cause, Cause::FormatContainer);
    assert_eq!(pad.lossiness(), Lossiness::Lossless);
    assert_eq!(p.target.sample, OutputSampleFormat::S32Le);
    // ZeroPad переквантования не даёт (§6.3, ТЗ-8).
    assert!(!requantizes(&p));
}

/// §7.2, ТЗ-28: устройства {16}, {16, 24(3)}, {16, 24-в-32}, {16, 32} ×
/// треки 16/24 — усечение только 24 на {16}; в остальных случаях выбирается
/// первый формат с `field_bits ≥ need` (§6.5).
#[test]
fn optimal_format_no_needless_truncation() {
    use OutputSampleFormat::{S16Le, S24Le, S24_3Le, S32Le};

    let s = settings();
    let devices: [&[OutputSampleFormat]; 4] = [&[S16Le], &[S16Le, S24_3Le], &[S16Le, S24Le], &[S16Le, S32Le]];
    // (устройство, трек, ожидаемый формат, ожидаемые преобразования формата).
    let cases: Vec<(usize, u8, OutputSampleFormat, Vec<TransformKind>)> = vec![
        (0, 16, S16Le, vec![]),
        (0, 24, S16Le, vec![TransformKind::Truncate(Truncate::new(bd(24), bd(16)).unwrap())]),
        (1, 16, S16Le, vec![]),
        (1, 24, S24_3Le, vec![]),
        (2, 16, S16Le, vec![]),
        // ОВ-32: 24 бита в `S24_LE` — поле равно сэмплу, преобразования нет.
        (2, 24, S24Le, vec![]),
        (3, 16, S16Le, vec![]),
        (3, 24, S32Le, vec![TransformKind::ZeroPad(ZeroPad::new(bd(24), bd(32)).unwrap())]),
    ];
    for (dev, bits, want_fmt, want_kinds) in cases {
        let formats = devices[dev];
        let c = caps_exclusive(rates(&[48_000]), formats);
        let p = plan(&src_pcm(bits, 48_000, 2), &c, PathPolicy::Optimal(&s.optimal))
            .unwrap_or_else(|b| panic!("{formats:?} × {bits}: Blocked {b:?}"));
        let ctx = format!("{formats:?} × {bits}");
        assert_eq!(p.target.sample, want_fmt, "{ctx}");
        assert_eq!(kinds(&p), want_kinds.iter().collect::<Vec<_>>(), "{ctx}");
        let truncates = p.transforms.iter().any(|t| matches!(t.kind, TransformKind::Truncate(_)));
        assert_eq!(truncates, dev == 0 && bits == 24, "{ctx}: лишнее усечение");
        if truncates {
            assert_eq!(p.transforms[0].cause, Cause::DeviceLimit, "{ctx}");
        }
        // Переквантование (и TPDF в `with_gain`) — только при усечении (ТЗ-8).
        assert_eq!(requantizes(&p), truncates, "{ctx}");
    }
}

/// §7.2, ТЗ-35: Строгий, 24/192 стерео на устройстве с 192 кГц и `S24_LE` —
/// путь без преобразований.
#[test]
fn strict_passthrough_24_192() {
    let s = settings();
    let c = caps_exclusive(rates(&[96_000, 192_000]), &[OutputSampleFormat::S24Le, OutputSampleFormat::S32Le]);
    let p = plan(&src_pcm(24, 192_000, 2), &c, PathPolicy::Strict(&s.strict)).unwrap();
    assert!(p.transforms.is_empty(), "лишние преобразования: {:?}", p.transforms);
    assert_eq!(p.target.rate, sr(192_000));
    assert_eq!(p.target.sample, OutputSampleFormat::S24Le);
    assert_eq!(p.target.channels.get(), 2);
    assert!(!requantizes(&p));
    // Бейдж «Bit-perfect» при `Confirmed` зависит от `OpenedFacts` после
    // открытия устройства и проверяется в тестах вердикта: план даёт только
    // необходимое условие — нет преобразований с причиной бейджа.
}

/// §7.2, ТЗ-37: Строгий, 24 бита на {16} — `Blocked { at: Output,
/// TruncationForbidden { from: 24, max: 16 } }`; 16 бит на {32} —
/// воспроизведение с `ZeroPad 16→32` без потерь.
#[test]
fn strict_truncation_is_incompatible() {
    let s = settings();
    let c = caps_exclusive(rates(&[48_000]), &[OutputSampleFormat::S16Le]);
    let err = plan(&src_pcm(24, 48_000, 2), &c, PathPolicy::Strict(&s.strict)).unwrap_err();
    let Blocked { at, reason, partial } = err;
    assert_eq!(at, StageId::Output);
    assert_eq!(reason, Incompatibility::TruncationForbidden { from: bd(24), max: 16 });
    assert!(
        !partial.transforms.iter().any(|t| matches!(t.kind, TransformKind::Truncate(_))),
        "Truncate в partial: {:?}",
        partial.transforms
    );
    // Текст «усечение 24→16 запрещено режимом» (ТЗ-37) проверяется в шаге
    // отчёта (AM1.0-8.5 шаг 22): у `Incompatibility` пока нет `Display`.

    let c = caps_exclusive(rates(&[48_000]), &[OutputSampleFormat::S32Le]);
    let p = plan(&src_pcm(16, 48_000, 2), &c, PathPolicy::Strict(&s.strict)).unwrap();
    let pad = TransformKind::ZeroPad(ZeroPad::new(bd(16), bd(32)).unwrap());
    assert_eq!(kinds(&p), vec![&pad], "ожидался один ZeroPad 16→32");
    assert_eq!(pad.lossiness(), Lossiness::Lossless);
    assert_eq!(p.target.sample, OutputSampleFormat::S32Le);
}

/// Lossy-источник MP3 стерео: выход декодера — 24-битные целые (ОВ-36).
fn src_mp3(rate: u32) -> SourceFormat {
    SourceFormat {
        container: Container::Mp3,
        codec: Codec::Mp3,
        lossy: true,
        rate: sr(rate),
        layout: ChannelLayout::stereo(),
        kind: SourceKind::Pcm { bits: bd(24) },
    }
}

/// Float-источник WAV стерео: float32 (`double = false`) или float64.
fn src_float(double: bool, rate: u32) -> SourceFormat {
    SourceFormat {
        container: Container::Wav,
        codec: Codec::Wav,
        lossy: false,
        rate: sr(rate),
        layout: ChannelLayout::stereo(),
        kind: SourceKind::FloatPcm { double },
    }
}

/// §7.2, ТЗ-22, ТЗ-53: Совместимый, 32-битный источник на сервере с `F32` —
/// единственное `Int32ToFloat` с потерями (мантисса float32 — 24 бита),
/// причина бейджа — `Int32ToFloat`.
#[test]
fn compatible_int32_source_marks_float_lossy() {
    let s = settings();
    let c = caps_shared(rates(&[48_000]), &[OutputSampleFormat::F32]);
    let p = plan(&src_pcm(32, 48_000, 2), &c, PathPolicy::Compatible(&s.compatible)).unwrap();
    assert_eq!(kinds(&p), vec![&TransformKind::Int32ToFloat], "ожидался только Int32ToFloat");
    assert_eq!(p.transforms[0].cause, Cause::ServerLimit);
    assert_eq!(p.transforms[0].kind.lossiness(), Lossiness::Lossy);
    assert_eq!(p.transforms[0].kind.badge_reason(), Some(NbpReason::Int32ToFloat));
    assert_eq!(p.target.sample, OutputSampleFormat::F32);
    // Выход `F32` переквантования не даёт (§6.3).
    assert!(!requantizes(&p));
}

/// §7.2, ТЗ-35, ТЗ-74, ТЗ-75, ОВ-36: MP3 — `need = 24` во всех режимах.
/// Строгий на {24} — passthrough; на {16} — `Blocked(TruncationForbidden
/// { from: 24, max: 16 })`; при достаточной разрядности ни в одном режиме нет
/// округления (`Truncate`/переквантования).
#[test]
fn lossy_source_is_24bit_in_all_modes() {
    let s = settings();
    let src = src_mp3(44_100);

    // Строгий на {24}: путь без преобразований.
    let c = caps_exclusive(rates(&[44_100]), &[OutputSampleFormat::S24Le]);
    let p = plan(&src, &c, PathPolicy::Strict(&s.strict)).unwrap();
    assert!(p.transforms.is_empty(), "лишние преобразования: {:?}", p.transforms);
    assert_eq!(p.target.sample, OutputSampleFormat::S24Le);
    assert!(!requantizes(&p));

    // Строгий на {16}: усечение 24→16 запрещено.
    let c = caps_exclusive(rates(&[44_100]), &[OutputSampleFormat::S16Le]);
    let err = plan(&src, &c, PathPolicy::Strict(&s.strict)).unwrap_err();
    let Blocked { at, reason, .. } = err;
    assert_eq!(at, StageId::Output);
    assert_eq!(reason, Incompatibility::TruncationForbidden { from: bd(24), max: 16 });

    // «Обработка» без округления: Совместимый на `F32` — `IntToFloat { 24 }`
    // без потерь; Оптимальный и Строгий на {16, 24} — `S24_LE` без
    // преобразований (16-битный формат не выбирается: `need = 24`).
    let shared = caps_shared(rates(&[44_100]), &[OutputSampleFormat::F32]);
    let excl = caps_exclusive(rates(&[44_100]), &[OutputSampleFormat::S16Le, OutputSampleFormat::S24Le]);
    for policy in all_policies(&s) {
        let compatible = matches!(policy, PathPolicy::Compatible(_));
        let c = if compatible { &shared } else { &excl };
        let p = plan(&src, c, policy).unwrap_or_else(|b| panic!("{policy:?}: Blocked {b:?}"));
        assert!(
            !p.transforms.iter().any(|t| matches!(t.kind, TransformKind::Truncate(_))),
            "{policy:?}: Truncate {:?}",
            p.transforms
        );
        assert!(!requantizes(&p), "{policy:?}: переквантование");
        if compatible {
            assert_eq!(kinds(&p), vec![&TransformKind::IntToFloat { bits: bd(24) }], "{policy:?}");
            assert_eq!(p.target.sample, OutputSampleFormat::F32, "{policy:?}");
        } else {
            assert!(p.transforms.is_empty(), "{policy:?}: лишние преобразования {:?}", p.transforms);
            assert_eq!(p.target.sample, OutputSampleFormat::S24Le, "{policy:?}");
        }
    }
}

/// §7.2, ТЗ-132, ОВС-1: WAV float. Совместимый: float32 — без
/// преобразований (выход `F32`, ring `F32`: побитово), float64 —
/// `FloatNarrow` (бейдж `Truncation`). Оптимальный — `FloatToInt` в
/// наибольший формат устройства с переквантованием. Строгий —
/// `Blocked { at: Output, FloatUnsupported }`.
#[test]
fn float_pcm_source_per_mode() {
    use OutputSampleFormat::{S16Le, S24Le, S32Le, F32};

    let s = settings();

    // Совместимый, float32: ни одного преобразования, нагрузка и выход `F32`.
    let c = caps_shared(rates(&[48_000]), &[F32]);
    let p = plan(&src_float(false, 48_000), &c, PathPolicy::Compatible(&s.compatible)).unwrap();
    assert!(p.transforms.is_empty(), "лишние преобразования: {:?}", p.transforms);
    assert_eq!(p.target.sample, F32);
    assert_eq!(p.ring, RingPayload::F32);
    assert!(!requantizes(&p));

    // Совместимый, float64: `FloatNarrow` с потерями.
    let p = plan(&src_float(true, 48_000), &c, PathPolicy::Compatible(&s.compatible)).unwrap();
    assert_eq!(kinds(&p), vec![&TransformKind::FloatNarrow], "ожидался только FloatNarrow");
    assert_eq!(p.transforms[0].cause, Cause::ServerLimit);
    assert_eq!(p.transforms[0].kind.lossiness(), Lossiness::Lossy);
    assert_eq!(p.transforms[0].kind.badge_reason(), Some(NbpReason::Truncation));
    assert_eq!(p.target.sample, F32);

    // Оптимальный: `FloatToInt { 32 }` в наибольший формат {16, 24, 32}.
    let c = caps_exclusive(rates(&[48_000]), &[S16Le, S24Le, S32Le]);
    for double in [false, true] {
        let p = plan(&src_float(double, 48_000), &c, PathPolicy::Optimal(&s.optimal)).unwrap();
        assert_eq!(kinds(&p), vec![&TransformKind::FloatToInt { bits: 32 }], "double={double}");
        assert_eq!(p.transforms[0].cause, Cause::DeviceLimit, "double={double}");
        assert_eq!(p.transforms[0].kind.badge_reason(), Some(NbpReason::FloatToInt), "double={double}");
        assert_eq!(p.target.sample, S32Le, "double={double}");
        assert_eq!(p.ring, RingPayload::F32, "double={double}");
        // Квантование `F32` → `S32_LE`: `with_gain` добавляет TPDF (§6.3 шаг 6, ТЗ-8).
        assert!(requantizes(&p), "double={double}");
        assert!(gets_tpdf(&p, PathPolicy::Optimal(&s.optimal)), "double={double}");
    }

    // Строгий: float-источник несовместим (ТЗ-132).
    for double in [false, true] {
        let err = plan(&src_float(double, 48_000), &c, PathPolicy::Strict(&s.strict)).unwrap_err();
        let Blocked { at, reason, .. } = err;
        assert_eq!(at, StageId::Output, "double={double}");
        assert_eq!(reason, Incompatibility::FloatUnsupported, "double={double}");
    }
}

/// §7.2, ТЗ-28, ОВС-3: Оптимальный, 16-битный трек на {16, 32}. С SRC
/// (`F32`-нагрузка) — наибольший формат `S32_LE` с переквантованием (TPDF);
/// без SRC — `S16_LE` без преобразований.
#[test]
fn optimal_dsp_path_uses_largest_format() {
    use OutputSampleFormat::{S16Le, S32Le};

    let s = settings();

    // С SRC 44.1→48: единственное преобразование — `Resample`, выход `S32_LE`.
    let c = caps_exclusive(rates(&[48_000]), &[S16Le, S32Le]);
    let p = plan(&src_pcm(16, 44_100, 2), &c, PathPolicy::Optimal(&s.optimal)).unwrap();
    assert_eq!(single_resample(&p), (sr(44_100), sr(48_000), Cause::DeviceLimit));
    assert_eq!(p.transforms.len(), 1, "лишние преобразования: {:?}", p.transforms);
    assert_eq!(p.target.sample, S32Le);
    assert_eq!(p.ring, RingPayload::F32);
    // Переквантование `F32` → `S32_LE`: `with_gain` добавляет TPDF (§6.3 шаг 6).
    assert!(requantizes(&p));
    assert!(gets_tpdf(&p, PathPolicy::Optimal(&s.optimal)));

    // Без SRC: `S16_LE`, ни одного преобразования, без переквантования.
    let c = caps_exclusive(rates(&[44_100]), &[S16Le, S32Le]);
    let p = plan(&src_pcm(16, 44_100, 2), &c, PathPolicy::Optimal(&s.optimal)).unwrap();
    assert!(p.transforms.is_empty(), "лишние преобразования: {:?}", p.transforms);
    assert_eq!(p.target.sample, S16Le);
    assert_eq!(p.ring, RingPayload::ExactI32 { valid_bits: bd(16) });
    assert!(!requantizes(&p));
}

/// §7.2, ТЗ-22, ОВС-4: Совместимый, сервер без `F32`. На {16, 32} 24-битный
/// трек → `S32_LE` + `ZeroPad 24→32` без потерь; только {16} → `S16_LE` +
/// `Truncate 24→16` (`ServerLimit`) с переквантованием.
#[test]
fn compatible_int_output_when_no_f32() {
    use OutputSampleFormat::{S16Le, S32Le};

    let s = settings();

    let c = caps_shared(rates(&[48_000]), &[S16Le, S32Le]);
    let p = plan(&src_pcm(24, 48_000, 2), &c, PathPolicy::Compatible(&s.compatible)).unwrap();
    let pad = TransformKind::ZeroPad(ZeroPad::new(bd(24), bd(32)).unwrap());
    assert_eq!(kinds(&p), vec![&pad], "ожидался один ZeroPad 24→32");
    assert_eq!(p.transforms[0].cause, Cause::FormatContainer);
    assert_eq!(pad.lossiness(), Lossiness::Lossless);
    assert_eq!(p.target.sample, S32Le);
    assert!(!requantizes(&p));

    let c = caps_shared(rates(&[48_000]), &[S16Le]);
    let p = plan(&src_pcm(24, 48_000, 2), &c, PathPolicy::Compatible(&s.compatible)).unwrap();
    let cut = TransformKind::Truncate(Truncate::new(bd(24), bd(16)).unwrap());
    assert_eq!(kinds(&p), vec![&cut], "ожидался один Truncate 24→16");
    assert_eq!(p.transforms[0].cause, Cause::ServerLimit);
    assert_eq!(cut.badge_reason(), Some(NbpReason::Truncation));
    assert_eq!(p.target.sample, S16Le);
    // Переквантование при `Truncate`: `with_gain` добавляет TPDF (§6.3, ТЗ-8).
    assert!(requantizes(&p));
    assert!(gets_tpdf(&p, PathPolicy::Compatible(&s.compatible)));
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

/// Статусы шагов DSD-цепочки плана в порядке Native → DoP → PCM (§6.9, ТЗ-40).
fn chain(p: &PathPlan) -> Vec<DsdChainStep> {
    p.dsd_chain.as_ref().expect("у DSD-плана есть dsd_chain").steps.to_vec()
}

/// Ожидаемая цепочка Native → DoP → PCM.
fn want_chain(native: DsdStepStatus, dop: DsdStepStatus, pcm: DsdStepStatus) -> Vec<DsdChainStep> {
    vec![
        DsdChainStep { kind: DsdStepKind::Native, status: native },
        DsdChainStep { kind: DsdStepKind::Dop, status: dop },
        DsdChainStep { kind: DsdStepKind::Pcm, status: pcm },
    ]
}

/// Частоты PCM-контейнера DoP для DSD64/128/256 (§6.9: `dsd / 16`).
const DOP_RATES_64_256: [u32; 3] = [176_400, 352_800, 705_600];

/// Проверка маршрута `DsdToPcm`: единственное преобразование `DsdToPcm` с
/// причиной `cause`, частотой `rate` и границей `stop_hz`; маршрут совпадает
/// с преобразованием; нагрузка ring — `F32` (§6.3 шаг 7, ОВС-10).
fn assert_dsd_to_pcm(p: &PathPlan, dsd: DsdRate, rate: u32, stop_hz: u32, cause: Cause, ctx: &str) {
    let filter = DsdFilter::default();
    let want_route = Route::DsdToPcm { dsd, pcm_rate: sr(rate), filter, stop_hz, gain_comp: true };
    assert_eq!(p.route, want_route, "{ctx}");
    let want = TransformKind::DsdToPcm { dsd, rate: sr(rate), filter, stop_hz, gain_comp: true };
    assert_eq!(kinds(p), vec![&want], "{ctx}: ожидался только DsdToPcm");
    assert_eq!(p.transforms[0].cause, cause, "{ctx}");
    assert_eq!(p.transforms[0].kind.lossiness(), Lossiness::Lossy, "{ctx}");
    assert_eq!(p.transforms[0].kind.badge_reason(), Some(NbpReason::DsdToPcm), "{ctx}");
    assert_eq!(p.target.rate, sr(rate), "{ctx}");
    assert_eq!(p.target.channels.get(), 2, "{ctx}");
    assert_eq!(p.ring, RingPayload::F32, "{ctx}");
}

/// §7.2, ТЗ-23, ТЗ-1: Совместимый — DSD64/128/256/512 только через
/// `DsdToPcm` (`ModeRule`); Native и DoP «отключено режимом», даже если
/// сервер сообщает частоты DoP и 24-битные форматы.
#[test]
fn compatible_dsd_only_pcm() {
    use DsdStepStatus::{Chosen, DisabledByMode};

    let s = settings();
    let c = caps_shared(
        rates(&[44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 705_600]),
        &[OutputSampleFormat::F32, OutputSampleFormat::S24Le, OutputSampleFormat::S32Le],
    );
    for d in DsdRate::ALL {
        let p = plan(&src_dsd(d), &c, PathPolicy::Compatible(&s.compatible))
            .unwrap_or_else(|b| panic!("{d:?}: Blocked {b:?}"));
        let ctx = format!("{d:?}");
        assert_eq!(chain(&p), want_chain(DisabledByMode, DisabledByMode, Chosen), "{ctx}");
        // DSD64 с фильтром 30 кГц (по умолчанию) → 88,2 кГц, прочие → 176,4 кГц (§6.9).
        let rate = if d == DsdRate::Dsd64 { 88_200 } else { 176_400 };
        assert_dsd_to_pcm(&p, d, rate, 30_000, Cause::ModeRule, &ctx);
        assert_eq!(p.target.sample, OutputSampleFormat::F32, "{ctx}");
        assert!(!requantizes(&p), "{ctx}: выход F32 не переквантуется");
    }
}

/// §7.2, ТЗ-31: Оптимальный — DSD64 на 176,4 кГц с `S24_LE` (24-в-32) →
/// DoP; без 176,4 кГц → DSD→PCM (`DeviceLimit`); Native «пока не
/// реализовано».
#[test]
fn optimal_dsd_chain_dop_then_pcm() {
    use DsdStepStatus::{Chosen, NotImplemented, NotTried, Unsupported};
    use OutputSampleFormat::{S24Le, S32Le};

    let s = settings();

    // Ветка DoP: цель целиком задаёт маршрут (ADR-12).
    let c = caps_exclusive(rates(&[44_100, 88_200, 176_400]), &[S24Le, S32Le]);
    let p = plan(&src_dsd(DsdRate::Dsd64), &c, PathPolicy::Optimal(&s.optimal)).unwrap();
    assert_eq!(chain(&p), want_chain(NotImplemented, Chosen, NotTried));
    let Route::Dop(route) = p.route else {
        panic!("ожидался Route::Dop: {:?}", p.route);
    };
    assert_eq!(route.dsd(), DsdRate::Dsd64);
    assert_eq!(route.pcm_rate(), sr(176_400));
    assert_eq!(kinds(&p), vec![&TransformKind::DopPack { dsd: DsdRate::Dsd64 }], "ожидался только DopPack");
    assert_eq!(p.transforms[0].cause, Cause::ModeRule);
    assert_eq!(p.transforms[0].kind.lossiness(), Lossiness::Lossless);
    assert_eq!(p.transforms[0].kind.badge_reason(), None);
    assert_eq!(p.target.rate, sr(176_400));
    assert_eq!(p.target.sample, S24Le);
    assert_eq!(p.target.channels.get(), 2);
    assert_eq!(p.ring, RingPayload::ExactI32 { valid_bits: bd(24) });
    assert!(!requantizes(&p));

    // Ветка PCM: DoP невозможен (нет 176,4 кГц) — причина «устройство не
    // поддерживает DoP» (`DeviceLimit`), а не `DsdAboveDac`.
    let c = caps_exclusive(rates(&[44_100, 88_200]), &[S24Le, S32Le]);
    let p = plan(&src_dsd(DsdRate::Dsd64), &c, PathPolicy::Optimal(&s.optimal)).unwrap();
    assert_eq!(chain(&p), want_chain(NotImplemented, Unsupported, Chosen));
    assert_dsd_to_pcm(&p, DsdRate::Dsd64, 88_200, 30_000, Cause::DeviceLimit, "без 176.4");
    // `F32`-нагрузка DSD→PCM — наибольший формат устройства (§6.5);
    // переквантование в целый выход — `with_gain` добавляет TPDF (§6.3 шаг 6).
    assert_eq!(p.target.sample, S32Le);
    assert!(requantizes(&p));
    assert!(gets_tpdf(&p, PathPolicy::Optimal(&s.optimal)));
}

/// §7.2, ТЗ-33: Оптимальный — DSD512 при DoP до DSD256 → DSD→PCM с причиной
/// `PlayerSetting(DsdAboveDac)` (`DsdAboveDac::ConvertToPcm`); DSD256 на том
/// же устройстве идёт через DoP.
#[test]
fn optimal_dsd_above_dac_converts() {
    use DsdStepStatus::{Chosen, NotImplemented, Unsupported};
    use OutputSampleFormat::{S24Le, S32Le};

    let s = settings();
    let c = caps_exclusive(rates(&DOP_RATES_64_256), &[S24Le, S32Le]);

    let p = plan(&src_dsd(DsdRate::Dsd512), &c, PathPolicy::Optimal(&s.optimal)).unwrap();
    assert_eq!(chain(&p), want_chain(NotImplemented, Unsupported, Chosen));
    assert_dsd_to_pcm(
        &p,
        DsdRate::Dsd512,
        176_400,
        30_000,
        Cause::PlayerSetting(ParamId::DsdAboveDac),
        "DSD512 на max DSD256",
    );
    assert_eq!(p.target.sample, S32Le);
    // Текст причины «DSD512 выше возможностей устройства (max DSD256)»
    // проверяется в шаге отчёта (AM1.0-8.5 шаг 22): здесь — вариант `Cause`.

    let p = plan(&src_dsd(DsdRate::Dsd256), &c, PathPolicy::Optimal(&s.optimal)).unwrap();
    assert!(matches!(p.route, Route::Dop(r) if r.dsd() == DsdRate::Dsd256), "DSD256: {:?}", p.route);
    assert_eq!(p.target.rate, sr(705_600));
}

/// §7.2, ТЗ-39, ТЗ-40: Строгий — DSD64 на DoP 176,4 кГц → DoP; DSD512 без
/// DoP512 → `Blocked { at: Output, DopUnsupported { Dsd512, max_dop } }`;
/// DSD→PCM не выбирается («отключено режимом»); Native не пробуется.
#[test]
fn strict_dsd_chain_allows_native_to_dop_forbids_pcm() {
    use DsdStepStatus::{Chosen, DisabledByMode, NotImplemented, Unsupported};
    use OutputSampleFormat::{S24Le, S32Le};

    let s = settings();

    let c = caps_exclusive(rates(&[176_400]), &[S24Le, S32Le]);
    let p = plan(&src_dsd(DsdRate::Dsd64), &c, PathPolicy::Strict(&s.strict)).unwrap();
    assert_eq!(chain(&p), want_chain(NotImplemented, Chosen, DisabledByMode));
    assert!(matches!(p.route, Route::Dop(r) if r.dsd() == DsdRate::Dsd64), "{:?}", p.route);
    assert_eq!(kinds(&p), vec![&TransformKind::DopPack { dsd: DsdRate::Dsd64 }]);
    assert_eq!(p.target.rate, sr(176_400));
    assert_eq!(p.target.sample, S24Le);
    assert_eq!(p.ring, RingPayload::ExactI32 { valid_bits: bd(24) });

    // (устройство, ожидаемый max_dop): DoP до DSD256 и устройство без DoP.
    let cases = [
        (caps_exclusive(rates(&DOP_RATES_64_256), &[S24Le, S32Le]), Some(DsdRate::Dsd256)),
        (caps_exclusive(rates(&[44_100, 88_200]), &[S24Le, S32Le]), None),
    ];
    for (c, max_dop) in cases {
        let err = plan(&src_dsd(DsdRate::Dsd512), &c, PathPolicy::Strict(&s.strict)).unwrap_err();
        let Blocked { at, reason, partial } = err;
        assert_eq!(at, StageId::Output, "max_dop={max_dop:?}");
        assert_eq!(reason, Incompatibility::DopUnsupported { dsd: DsdRate::Dsd512, max_dop });
        // Native — «пока не реализовано» (попыток 0), PCM — «отключено режимом».
        assert_eq!(chain(&partial), want_chain(NotImplemented, Unsupported, DisabledByMode));
        assert!(!matches!(partial.route, Route::DsdToPcm { .. }), "DSD→PCM выбран: {:?}", partial.route);
        assert!(
            !partial.transforms.iter().any(|t| matches!(t.kind, TransformKind::DsdToPcm { .. })),
            "DsdToPcm в partial: {:?}",
            partial.transforms
        );
    }
}

/// §7.2, ТЗ-16, ТЗ-128, И-Р11: частота DSD→PCM — DSD64 → 88,2; DSD64 +
/// 50 кГц → 176,4; DSD128/256/512 → 176,4; DSD256 на max 96 → 88,2; DSD64 +
/// 50 кГц на max 96 → 88,2 и `stop_hz = 44 100`. План использует ту же
/// функцию `dsd_pcm_rate`, что декодер и окно.
#[test]
fn dsd_pcm_rate_rule() {
    use DsdFilter::{K30, K50};
    use DsdRate::{Dsd128, Dsd256, Dsd512, Dsd64};

    let full = caps_shared(rates(&[44_100, 48_000, 88_200, 96_000, 176_400, 192_000]), &[OutputSampleFormat::F32]);
    let max96 = caps_shared(rates(&[44_100, 48_000, 88_200, 96_000]), &[OutputSampleFormat::F32]);
    // (устройство, DSD, фильтр, частота, действующая граница фильтра).
    let cases = [
        (&full, Dsd64, K30, 88_200, 30_000),
        (&full, Dsd64, K50, 176_400, 50_000),
        (&full, Dsd128, K30, 176_400, 30_000),
        (&full, Dsd256, K30, 176_400, 30_000),
        (&full, Dsd512, K30, 176_400, 30_000),
        (&max96, Dsd256, K30, 88_200, 30_000),
        (&max96, Dsd64, K50, 88_200, 44_100),
    ];
    for (c, d, filter, rate, stop_hz) in cases {
        let ctx = format!("{d:?} {filter:?} {:?}", c.rates);
        assert_eq!(dsd_pcm_rate(d, filter, c), sr(rate), "{ctx}");
        assert_eq!(dsd_stop_hz(filter, sr(rate)), stop_hz, "{ctx}");

        let mut s = settings();
        s.compatible.dsd_filter = filter;
        let p = plan(&src_dsd(d), c, PathPolicy::Compatible(&s.compatible)).unwrap();
        let Route::DsdToPcm { pcm_rate, stop_hz: route_stop, filter: route_filter, .. } = p.route else {
            panic!("{ctx}: ожидался DsdToPcm, {:?}", p.route);
        };
        assert_eq!(pcm_rate, sr(rate), "{ctx}");
        assert_eq!(route_stop, stop_hz, "{ctx}");
        assert_eq!(route_filter, filter, "{ctx}");
        assert_eq!(p.target.rate, sr(rate), "{ctx}: лишний SRC");
        assert!(resamples(&p).is_empty(), "{ctx}: лишний Resample {:?}", p.transforms);
    }
    // Заметка «фильтр 50 кГц → фактически 44.1 кГц» (`DsdToPcm::describe()`)
    // проверяется в шаге отчёта (AM1.0-8.5 шаг 22).
}

/// §7.2, ТЗ-1, И-Т3: на Shared-устройстве `plan` никогда не возвращает
/// `Route::Dop` и `DopPack` — ни в одном режиме, ни для одной DSD-частоты,
/// даже при частотах DoP и 24-битных форматах сервера.
#[test]
fn validate_dop_shared_is_unsupported() {
    use OutputSampleFormat::{S24Le, S24_3Le, S32Le, F32};

    let s = settings();
    let c = caps_shared(
        rates(&[88_200, 176_400, 352_800, 705_600, 1_411_200]),
        &[F32, S24_3Le, S24Le, S32Le],
    );
    for policy in all_policies(&s) {
        for d in DsdRate::ALL {
            let ctx = format!("{policy:?} {d:?}");
            let p = match plan(&src_dsd(d), &c, policy) {
                Ok(p) => p,
                // Строгий: DoP недоступен (`max_dop` для Shared всегда `None`).
                Err(Blocked { reason, partial, .. }) => {
                    assert!(matches!(policy, PathPolicy::Strict(_)), "{ctx}: Blocked {reason:?}");
                    assert_eq!(reason, Incompatibility::DopUnsupported { dsd: d, max_dop: None }, "{ctx}");
                    partial
                }
            };
            assert!(!matches!(p.route, Route::Dop(_)), "{ctx}: Route::Dop на Shared");
            assert!(
                !p.transforms.iter().any(|t| matches!(t.kind, TransformKind::DopPack { .. })),
                "{ctx}: DopPack на Shared {:?}",
                p.transforms
            );
        }
    }
    // Вторая часть строки («`SharedRender` не содержит DoP») обеспечена типом:
    // `SharedRender` оборачивает только `PcmRenderF32Out` (backend/shared.rs),
    // DoP-варианта нет — проверяется компиляцией, а не тестом.
}

fn vol(v: u8) -> Volume {
    Volume::new(v).unwrap()
}

/// Число преобразований вида `pred` в плане.
fn count(p: &PathPlan, pred: impl Fn(&TransformKind) -> bool) -> usize {
    p.transforms.iter().filter(|t| pred(&t.kind)).count()
}

/// §7.2, ТЗ-35, §6.3 «Свойство Строгого режима»: декартово произведение
/// источников (частоты × разрядности × каналы, float-PCM, DSD, lossy) на
/// устройства (Shared и Exclusive, разные частоты, форматы, каналы, с DoP и
/// без). Каждый `Ok` в Строгом режиме — только преобразования без потерь, и
/// так же после `with_gain` с mute и без него; иначе — `Blocked`.
#[test]
fn strict_never_lossy_property() {
    use OutputSampleFormat::{S16Le, S24Le, S24_3Le, S32Le, F32};

    const PCM_RATES: [u32; 7] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 384_000];
    let mut sources = Vec::new();
    for rate in PCM_RATES {
        for bits in [16, 24, 32] {
            for ch in [1, 2, 6] {
                sources.push(src_pcm(bits, rate, ch));
            }
        }
        sources.push(src_float(false, rate));
        sources.push(src_float(true, rate));
        sources.push(src_mp3(rate));
    }
    sources.push(src_51());
    sources.extend(DsdRate::ALL.into_iter().map(src_dsd));

    let all_rates = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 1_411_200];
    let range = RateSet::Range { min: sr(44_100), max: sr(384_000) };
    let format_sets: [&[OutputSampleFormat]; 5] =
        [&[S16Le], &[S16Le, S24Le], &[S24_3Le, S32Le], &[S16Le, S24Le, S32Le, F32], &[F32]];
    let rate_sets = [rates(&[44_100]), rates(&[48_000, 96_000]), rates(&DOP_RATES_64_256), rates(&all_rates), range];
    let channel_sets: [&[u8]; 4] = [&[2], &[1, 2], &[2, 6], &[8]];
    let mut devices = Vec::new();
    for r in &rate_sets {
        for f in format_sets {
            for ch in channel_sets {
                devices.push(with_channels(caps_exclusive(r.clone(), f), ch));
                devices.push(with_channels(caps_shared(r.clone(), f), ch));
            }
        }
    }

    let s = settings();
    let policy = PathPolicy::Strict(&s.strict);
    let (mut ok, mut blocked) = (0usize, 0usize);
    for src in &sources {
        for c in &devices {
            let Ok(p) = plan(src, c, policy) else {
                blocked += 1;
                continue;
            };
            ok += 1;
            assert_eq!(p.mode, ModeKind::Strict);
            assert!(p.transforms.iter().all(|t| t.kind.allowed_in(ModeKind::Strict)), "{src:?} на {c:?}: {:?}", p.transforms);
            for gain in [GainState::NotApplicable, GainState::Unity, GainState::Muted, GainState::Scaled(vol(50))] {
                let g = with_gain(&p, gain, policy);
                assert!(
                    g.transforms.iter().all(|t| t.kind.allowed_in(ModeKind::Strict)),
                    "{src:?} на {c:?}, {gain:?}: {:?}",
                    g.transforms
                );
            }
        }
    }
    assert!(ok > 0 && blocked > 0, "вырожденный перебор: ok={ok}, blocked={blocked}");
}

/// §6.3 шаги 5–6, ТЗ-8, ОВ-5: Совместимый, целый выход. `Scaled(50)` →
/// `Volume` + `Dither(Tpdf)`; `Scaled(100)` и `Unity` — без `Volume` и без
/// дизеринга; при `Dither::Off` — `Volume` без `Dither`; повторный вызов не
/// дублирует преобразования.
#[test]
fn with_gain_volume_adds_dither_compatible() {
    use OutputSampleFormat::S16Le;

    let mut s = settings();
    let c = caps_shared(rates(&[44_100]), &[S16Le]);
    let p = plan(&src_pcm(16, 44_100, 2), &c, PathPolicy::Compatible(&s.compatible)).unwrap();
    assert!(p.transforms.is_empty(), "лишние преобразования: {:?}", p.transforms);
    let policy = PathPolicy::Compatible(&s.compatible);

    let g = with_gain(&p, GainState::Scaled(vol(50)), policy);
    assert_eq!(kinds(&g), vec![&TransformKind::Volume(vol(50)), &TransformKind::Dither(Dither::Tpdf)]);
    assert_eq!(g.transforms[0].cause, Cause::PlayerSetting(ParamId::Volume));
    assert_eq!(g.transforms[1].cause, Cause::PlayerSetting(ParamId::DitherKind));
    assert_eq!(g.target, p.target);
    assert_eq!(g.ring, p.ring);

    // Повторный вызов на результате идемпотентен; новая громкость заменяет `Volume`.
    assert_eq!(with_gain(&g, GainState::Scaled(vol(50)), policy), g);
    let g2 = with_gain(&g, GainState::Scaled(vol(30)), policy);
    assert_eq!(kinds(&g2), vec![&TransformKind::Volume(vol(30)), &TransformKind::Dither(Dither::Tpdf)]);

    // 100 % и Unity побитово прозрачны: ни `Volume`, ни `Dither` (И-Р19).
    for gain in [GainState::Scaled(vol(100)), GainState::Unity, GainState::NotApplicable] {
        assert!(with_gain(&g, gain, policy).transforms.is_empty(), "{gain:?}");
    }

    s.compatible.dither = Dither::Off;
    let policy = PathPolicy::Compatible(&s.compatible);
    let g = with_gain(&p, GainState::Scaled(vol(50)), policy);
    assert_eq!(kinds(&g), vec![&TransformKind::Volume(vol(50))]);
    assert_eq!(count(&with_gain(&g, GainState::Scaled(vol(50)), policy), |k| matches!(k, TransformKind::Volume(_))), 1);
}

/// §6.3 шаг 5, ОВС-18, ОВС-19, ADR-23: mute по стадии усиления.
/// `AtomicGain` (Совместимый, Оптимальный без фиксации) → `Mute`; `NoGain`
/// (Оптимальный с `volume_lock`, Строгий, DoP) → `SilenceGate`; `Scaled` в
/// стадии `NoGain` ничего не добавляет (ТЗ-41).
#[test]
fn with_gain_mute_by_stage() {
    use OutputSampleFormat::{S24Le, S32Le};

    let s = settings();
    let c = caps_exclusive(rates(&[44_100]), &[S24Le, S32Le]);
    let src = src_pcm(24, 44_100, 2);
    let is_mute = |k: &TransformKind| matches!(k, TransformKind::Mute);
    let is_gate = |k: &TransformKind| matches!(k, TransformKind::SilenceGate);

    for policy in [PathPolicy::Compatible(&s.compatible), PathPolicy::Optimal(&s.optimal)] {
        let p = plan(&src, &c, policy).unwrap();
        let g = with_gain(&p, GainState::Muted, policy);
        assert_eq!(kinds(&g).last(), Some(&&TransformKind::Mute), "{policy:?}");
        assert_eq!(g.transforms.last().unwrap().cause, Cause::PlayerSetting(ParamId::Mute));
        assert_eq!(count(&g, is_gate), 0, "{policy:?}");
        assert_eq!(count(&g, |k| matches!(k, TransformKind::Dither(_))), 0, "{policy:?}: дизеринг при mute");
        // Снятие mute убирает `Mute` (пересчёт шагов 5–6).
        assert_eq!(count(&with_gain(&g, GainState::Unity, policy), is_mute), 0, "{policy:?}");
    }

    let mut locked = settings();
    locked.optimal.volume_lock = true;
    let dop_caps = caps_exclusive(rates(&[176_400]), &[S24Le, S32Le]);
    let optimal = PathPolicy::Optimal(&s.optimal);
    let dop = plan(&src_dsd(DsdRate::Dsd64), &dop_caps, optimal).unwrap();
    assert!(matches!(dop.route, Route::Dop(_)), "ожидался Route::Dop: {:?}", dop.route);
    let locked_policy = PathPolicy::Optimal(&locked.optimal);
    let strict = PathPolicy::Strict(&s.strict);
    let cases = [
        (locked_policy, plan(&src, &c, locked_policy).unwrap()),
        (strict, plan(&src, &c, strict).unwrap()),
        // DoP: стадии громкости нет даже в Оптимальном без фиксации (ADR-12).
        (optimal, dop),
    ];
    for (policy, p) in &cases {
        let g = with_gain(p, GainState::Muted, *policy);
        assert_eq!(kinds(&g).last(), Some(&&TransformKind::SilenceGate), "{policy:?} {:?}", p.route);
        assert_eq!(g.transforms.last().unwrap().cause, Cause::PlayerSetting(ParamId::Mute));
        assert_eq!(count(&g, is_mute), 0, "{policy:?}");
        assert_eq!(with_gain(p, GainState::Scaled(vol(50)), *policy), *p, "{policy:?}: Scaled в стадии NoGain");
    }
}
