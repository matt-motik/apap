//! Тесты планировщика `plan()` по таблице §7.2 (AM1.0 §6.4, §6.5, §6.13,
//! ТЗ-22, ТЗ-27, ТЗ-36, ТЗ-54, ТЗ-133, ОВ-3, ОВ-4, ОВС-2): выбор частоты и
//! правила каналов в трёх режимах на синтетических `DeviceCaps` — без
//! реального устройства (ТЗ-114).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use smallvec::SmallVec;

use crate::audio::backend::{AccessKind, DeviceCaps, OutputApi, RateSet};
use crate::audio::error::Incompatibility;
use crate::audio::format::{
    BitDepth, ChannelLayout, ChannelPos, Codec, Container, OutputSampleFormat, SampleRate, SourceFormat, SourceKind,
};
use crate::settings::params::ParamId;
use crate::settings::playback::{ModeSettings, PathPolicy, RateFallbackRule};

use crate::audio::verdict::NbpReason;

use super::super::transform::{Cause, Lossiness, Transform, TransformKind};
use super::super::{Blocked, PathPlan, StageId};
use super::plan;

fn sr(hz: u32) -> SampleRate {
    SampleRate::new(hz).unwrap()
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
    // Проверка формата сэмпла (только IntToFloat) — при реализации шага
    // формата, AM1.0-8.5 шаг 9.
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
