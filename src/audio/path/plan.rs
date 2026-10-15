//! Планировщик пути сигнала: `plan(src, caps, policy)` (AM1.0 §6.3, §6.4,
//! ТЗ-22, ТЗ-27, ТЗ-30, ТЗ-36, ТЗ-38, ОВ-3, ОВ-4).
//!
//! Чистая функция без I/O: по формату источника, возможностям устройства и
//! политике активного режима строит `PathPlan` или `Blocked` (Строгий режим).
//! Каждый шаг таблицы §6.3 — отдельная приватная функция, вызываемая строго
//! в порядке таблицы.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::cmp::Reverse;
use std::num::NonZeroU8;

use smallvec::SmallVec;

use crate::audio::backend::{AccessKind, DeviceCaps, RateSet};
use crate::audio::error::Incompatibility;
use crate::audio::format::{
    ChannelLayout, ChannelPos, OutputSampleFormat, SampleRate, SourceFormat, SourceKind, StreamFormat,
};
use crate::audio::session::RingPayload;
use crate::settings::params::ParamId;
use crate::settings::playback::{ModeKind, PathPolicy, RateFallbackRule};

use super::transform::{Cause, Confidence, Transform, TransformKind};
use super::{Blocked, PathPlan, Route, StageId};

/// Стандартный ряд частот для `RateSet::Range` (§6.4), Гц.
const STANDARD_RATES_HZ: [u32; 10] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
];

/// Несовместимость шага: этап, на котором план остановлен, и причина (§6.3).
type StepError = (StageId, Incompatibility);

/// Строит план пути сигнала (§6.3, ТЗ-38).
///
/// Шаги выполняются строго в порядке таблицы §6.3. При первой
/// несовместимости возвращается `Blocked { at, reason, partial }`, где
/// `partial` — план с результатами уже выполненных шагов (ТЗ-80).
///
/// Текущее состояние каркаса (этап AM1.0-8.5):
/// - шаг 1 (маршрут, §6.9): PCM-источник → `Route::Pcm`. DSD-маршрут (DoP,
///   DSD→PCM) строится на шаге 11 этапа; до него DSD-источник в любом режиме
///   даёт `Blocked { at: Output, reason: DopUnsupported { max_dop: None } }`;
/// - шаг 2 (каналы, §6.5, §6.13, ТЗ-54) реализован полностью: `MonoToStereo`,
///   `Downmix`, `ChannelPad` или `ChannelsUnsupported` в Строгом режиме;
/// - шаг 4 (формат сэмпла, §6.5) реализуется на шаге 8 этапа. Пока формат
///   сэмпла — точное совпадение с источником из `caps.formats`, иначе первый
///   формат устройства (`device_sample_format`); преобразования формата не добавляются;
/// - шаг 3 (частота, §6.4) реализован полностью;
/// - шаги 5–6 (усиление и дизеринг) выполняет не `plan()`, а
///   `with_gain(&PathPlan, GainState)` (§6.3, шаг 13 этапа): громкость
///   меняется без переоткрытия;
/// - шаг 7 (полезная нагрузка ring) реализован полностью.
///
/// Все преобразования планировщика имеют `Confidence::Assumed`: план строится
/// до открытия потока, а подтверждение появляется только после `configure`
/// при сверке с драйвером (§6.4 «Сверка с драйвером», ТЗ-30, ТЗ-113).
#[expect(
    clippy::result_large_err,
    reason = "сигнатура §6.3: Blocked несёт partial PathPlan (§3.1, ТЗ-80); вызывается раз на открытие трека"
)]
pub fn plan(src: &SourceFormat, caps: &DeviceCaps, policy: PathPolicy<'_>) -> Result<PathPlan, Blocked> {
    let mut draft = PathPlan {
        mode: mode_of(policy),
        source: src.clone(),
        route: Route::Pcm,
        transforms: SmallVec::new(),
        target: StreamFormat {
            rate: src.rate,
            sample: natural_format(src.kind),
            channels: src.layout.count(),
        },
        ring: RingPayload::F32,
        dsd_chain: None,
    };
    match run_steps(&mut draft, caps, policy) {
        Ok(()) => Ok(draft),
        Err((at, reason)) => Err(Blocked { at, reason, partial: draft }),
    }
}

/// Шаги §6.3 по порядку; шаги 5–6 — в `with_gain` (см. doc `plan`).
fn run_steps(draft: &mut PathPlan, caps: &DeviceCaps, policy: PathPolicy<'_>) -> Result<(), StepError> {
    step_route(draft)?;
    step_channels(draft, caps)?;
    step_rate(draft, caps, policy)?;
    step_sample_format(draft, caps);
    step_ring(draft);
    Ok(())
}

/// Режим, для которого строится план (ADR-04).
fn mode_of(policy: PathPolicy<'_>) -> ModeKind {
    match policy {
        PathPolicy::Compatible(_) => ModeKind::Compatible,
        PathPolicy::Optimal(_) => ModeKind::Optimal,
        PathPolicy::Strict(_) => ModeKind::Strict,
    }
}

/// Преобразование планировщика: до открытия потока достоверность —
/// `Assumed` (§6.4 «Сверка с драйвером»).
fn assumed(kind: TransformKind, cause: Cause) -> Transform {
    Transform { kind, cause, confidence: Confidence::Assumed }
}

// ---------------------------------------------------------------------------
// Шаг 1. Маршрут (§6.9)
// ---------------------------------------------------------------------------

/// Шаг 1 (§6.3): PCM-источник → `Route::Pcm`. DSD-маршрут (§6.9, ТЗ-23,
/// ТЗ-40) строится на шаге 11 этапа AM1.0-8.5; до него DSD-источник
/// останавливает план на этапе «Вывод» с `DopUnsupported` без известного
/// максимума DoP (`max_dop: None`).
fn step_route(draft: &mut PathPlan) -> Result<(), StepError> {
    match draft.source.kind {
        SourceKind::Pcm { .. } | SourceKind::FloatPcm { .. } => {
            draft.route = Route::Pcm;
            draft.dsd_chain = None;
            Ok(())
        }
        SourceKind::Dsd { dsd } => {
            Err((StageId::Output, Incompatibility::DopUnsupported { dsd, max_dop: None }))
        }
    }
}

// ---------------------------------------------------------------------------
// Шаг 2. Каналы (§6.5)
// ---------------------------------------------------------------------------

/// Шаг 2 (§6.3, §6.5, ТЗ-12, ТЗ-13, ТЗ-36, ТЗ-54, ТЗ-133, ОВ-2, ОВС-2).
///
/// `n` — каналы источника, `C` — `caps.channels` (нули отбрасываются):
/// 1. `n ∈ C` — без изменений;
/// 2. `n = 1`, `2 ∈ C` — `MonoToStereo` во всех режимах;
/// 3. `n > 1`, есть `c < n`: Совместимый и Оптимальный — `Downmix` в
///    раскладку `max{c ∈ C : c < n}` (§6.13); Строгий — `ChannelsUnsupported`
///    на этапе «Вывод»;
/// 4. все `c > n` — `ChannelPad { from: n, to: min C }` во всех режимах
///    (без потерь); для моно без `2 ∈ C` сначала `MonoToStereo`, затем
///    `ChannelPad { from: 2, to: min C }`.
///
/// Причина преобразований — `ServerLimit` для Shared и `DeviceLimit` для
/// Exclusive. Решение шага AM1.0-8.5: пустой `C` (устройство не сообщило
/// числа каналов) — без изменений, как пустой `caps.formats` на шаге 4.
fn step_channels(draft: &mut PathPlan, caps: &DeviceCaps) -> Result<(), StepError> {
    let n = draft.source.layout.count().get();
    draft.target.channels = draft.source.layout.count();
    let supported = || caps.channels.iter().copied().filter(|&c| c > 0);
    let Some(min_c) = supported().min() else {
        return Ok(());
    };
    if supported().any(|c| c == n) {
        return Ok(());
    }
    let cause = channel_cause(caps);
    if n == 1 {
        draft.transforms.push(assumed(TransformKind::MonoToStereo, cause.clone()));
        if supported().any(|c| c == 2) {
            set_target_channels(draft, 2);
            return Ok(());
        }
        // Правило 4 для моно: все c > 1 и 2 ∉ C, значит min C > 2.
        draft.transforms.push(assumed(TransformKind::ChannelPad { from: 2, to: min_c }, cause));
        set_target_channels(draft, min_c);
        return Ok(());
    }
    if let Some(to) = supported().filter(|&c| c < n).max() {
        if draft.mode == ModeKind::Strict {
            return Err((
                StageId::Output,
                Incompatibility::ChannelsUnsupported { src: n, supported: caps.channels.clone() },
            ));
        }
        let from = draft.source.layout.clone();
        draft
            .transforms
            .push(assumed(TransformKind::Downmix { from, to: default_layout(to) }, cause));
        set_target_channels(draft, to);
        return Ok(());
    }
    // Правило 4: n ∉ C и нет c < n, значит все c > n.
    draft.transforms.push(assumed(TransformKind::ChannelPad { from: n, to: min_c }, cause));
    set_target_channels(draft, min_c);
    Ok(())
}

/// Причина преобразования каналов (§6.5): ограничение сервера (Shared) или
/// устройства (Exclusive).
fn channel_cause(caps: &DeviceCaps) -> Cause {
    match caps.access {
        AccessKind::Shared => Cause::ServerLimit,
        AccessKind::Exclusive => Cause::DeviceLimit,
    }
}

/// Число каналов цели; `count > 0` гарантирует вызывающий (нули из `C`
/// отброшены, `n ≥ 1`).
fn set_target_channels(draft: &mut PathPlan, count: u8) {
    if let Some(ch) = NonZeroU8::new(count) {
        draft.target.channels = ch;
    }
}

/// Раскладка по умолчанию для числа каналов (§6.13, порядок
/// WAVEFORMATEXTENSIBLE): цель `Downmix`, пока раскладки устройства
/// неизвестны. Больше 8 каналов — позиции `Unknown(i)`, как у декодера
/// без маски каналов.
fn default_layout(count: u8) -> ChannelLayout {
    use ChannelPos::{BC, BL, BR, FC, FL, FR, LFE, SL, SR};
    let positions: SmallVec<[ChannelPos; 8]> = match count {
        1 => return ChannelLayout::mono(),
        2 => return ChannelLayout::stereo(),
        3 => SmallVec::from_slice(&[FL, FR, FC]),
        4 => SmallVec::from_slice(&[FL, FR, BL, BR]),
        5 => SmallVec::from_slice(&[FL, FR, FC, BL, BR]),
        6 => SmallVec::from_slice(&[FL, FR, FC, LFE, BL, BR]),
        7 => SmallVec::from_slice(&[FL, FR, FC, LFE, BC, SL, SR]),
        8 => SmallVec::from_slice(&[FL, FR, FC, LFE, BL, BR, SL, SR]),
        n => (0..n).map(ChannelPos::Unknown).collect(),
    };
    ChannelLayout::new(&positions).unwrap_or_else(ChannelLayout::stereo)
}

// ---------------------------------------------------------------------------
// Шаг 3. Частота (§6.4)
// ---------------------------------------------------------------------------

/// Шаг 3 (§6.3, §6.4, ТЗ-22, ТЗ-27, ТЗ-36, ОВ-3, ОВ-4).
///
/// - Совместимый: `target = fixed_rate.unwrap_or(src)`; если `target`
///   поддерживается и `≠ src` — `Resample` с
///   `Cause::PlayerSetting(ParamId::FixedRate)`. Если `target` не
///   поддерживается сервером — правило «то же семейство» по частотам сервера
///   с `Cause::ServerLimit`. Решение шага AM1.0-8.5: это же правило
///   применяется и к неподдерживаемой `fixed_rate` (семейство считается от
///   `fixed_rate`), причина — `ServerLimit`, так как итоговую частоту
///   определил сервер, а не настройка.
/// - Оптимальный: поддерживается — без SRC; иначе правило `rate_fallback`
///   с `Cause::DeviceLimit` (ОВ-3).
/// - Строгий: не поддерживается — `RateUnsupported` на этапе «Вывод» (ТЗ-36).
fn step_rate(draft: &mut PathPlan, caps: &DeviceCaps, policy: PathPolicy<'_>) -> Result<(), StepError> {
    let src_rate = draft.source.rate;
    let chosen = match policy {
        PathPolicy::Compatible(opts) => {
            let wanted = opts.fixed_rate.unwrap_or(src_rate);
            if caps.rates.contains(wanted) {
                let cause = Cause::PlayerSetting(ParamId::FixedRate);
                Some((wanted, cause, opts.src_filter))
            } else {
                let rate = fallback_rate(&caps.rates, RateFallbackRule::SameFamily, wanted, src_rate)?;
                Some((rate, Cause::ServerLimit, opts.src_filter))
            }
        }
        PathPolicy::Optimal(opts) => {
            if caps.rates.contains(src_rate) {
                None
            } else {
                let rate = fallback_rate(&caps.rates, opts.rate_fallback, src_rate, src_rate)?;
                Some((rate, Cause::DeviceLimit, opts.src_filter))
            }
        }
        PathPolicy::Strict(_) => {
            if caps.rates.contains(src_rate) {
                None
            } else {
                return Err(rate_unsupported(src_rate, &caps.rates));
            }
        }
    };
    if let Some((to, cause, filter)) = chosen {
        draft.target.rate = to;
        if to != src_rate {
            draft
                .transforms
                .push(assumed(TransformKind::Resample { from: src_rate, to, filter }, cause));
        }
    }
    Ok(())
}

/// Частота по правилу `rule` относительно `wanted` (§6.4). Если кандидатов
/// нет: у `Range` без стандартных частот внутри — `wanted`, приведённая в
/// диапазон (решение шага AM1.0-8.5); у пустого `Discrete` выбрать нечего —
/// `RateUnsupported` для частоты источника `src_rate`.
fn fallback_rate(
    set: &RateSet,
    rule: RateFallbackRule,
    wanted: SampleRate,
    src_rate: SampleRate,
) -> Result<SampleRate, StepError> {
    let cands = candidates(set);
    if let Some(rate) = apply_rule(rule, &cands, wanted) {
        return Ok(rate);
    }
    match set {
        RateSet::Range { min, max } => Ok(wanted.clamp(*min, *max)),
        RateSet::Discrete(_) => Err(rate_unsupported(src_rate, set)),
    }
}

/// Несовместимость частоты на этапе «Вывод» (§6.3, §6.4).
fn rate_unsupported(rate: SampleRate, set: &RateSet) -> StepError {
    (StageId::Output, Incompatibility::RateUnsupported { rate, supported: set.clone() })
}

/// Кандидаты для правил выбора (§6.4): `Discrete` — сам набор; `Range` —
/// стандартный ряд внутри диапазона.
fn candidates(set: &RateSet) -> SmallVec<[SampleRate; 16]> {
    match set {
        RateSet::Discrete(rates) => rates.clone(),
        RateSet::Range { min, max } => STANDARD_RATES_HZ
            .iter()
            .filter_map(|&hz| SampleRate::new(hz))
            .filter(|r| min <= r && r <= max)
            .collect(),
    }
}

/// Семейство частоты по §6.4.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    /// Кратные 11 025 Гц.
    F44k1,
    /// Кратные 8 000 Гц.
    F48k,
    Other,
}

/// Семейство частоты (§6.4): `F44k1`, если `r % 11025 == 0`; `F48k`, если
/// `r % 8000 == 0`; иначе `Other`. Делитель 8 000 — по §6.4 (а не 4 000, как
/// у `SampleRate::family`), поэтому классификация своя.
fn family(rate: SampleRate) -> Family {
    let hz = rate.hz();
    if hz.is_multiple_of(11_025) {
        Family::F44k1
    } else if hz.is_multiple_of(8_000) {
        Family::F48k
    } else {
        Family::Other
    }
}

/// Правило выбора частоты (ТЗ-27, ОВ-3); `None` — кандидатов нет.
fn apply_rule(rule: RateFallbackRule, cands: &[SampleRate], f: SampleRate) -> Option<SampleRate> {
    match rule {
        RateFallbackRule::SameFamily => same_family(cands, f),
        RateFallbackRule::Nearest => nearest(cands, f),
        RateFallbackRule::NoDownsample => no_downsample(cands, f),
    }
}

/// `SameFamily` (§6.4): `max{r ∈ S : fam(r) = fam(f), r ≤ f}`; если такой
/// нет — `min{r ∈ S : fam(r) = fam(f), r > f}`; если частот этого семейства
/// нет — `Nearest`.
fn same_family(cands: &[SampleRate], f: SampleRate) -> Option<SampleRate> {
    let fam = family(f);
    let same = || cands.iter().copied().filter(move |&r| family(r) == fam);
    same()
        .filter(|&r| r <= f)
        .max()
        .or_else(|| same().filter(|&r| r > f).min())
        .or_else(|| nearest(cands, f))
}

/// `Nearest` (§6.4): `argmin |r − f|`; при равенстве — большая.
fn nearest(cands: &[SampleRate], f: SampleRate) -> Option<SampleRate> {
    cands.iter().copied().min_by_key(|&r| (r.hz().abs_diff(f.hz()), Reverse(r)))
}

/// `NoDownsample` (§6.4): `min{r ∈ S : r ≥ f}`; если такой нет — `max S`.
fn no_downsample(cands: &[SampleRate], f: SampleRate) -> Option<SampleRate> {
    cands
        .iter()
        .copied()
        .filter(|&r| r >= f)
        .min()
        .or_else(|| cands.iter().copied().max())
}

// ---------------------------------------------------------------------------
// Шаг 4. Формат сэмпла (§6.5)
// ---------------------------------------------------------------------------

/// Формат сэмпла цели: точное совпадение с источником из
/// `caps.formats` (в порядке устройства), иначе первый формат устройства;
/// если устройство форматов не сообщило — естественный формат источника.
fn device_sample_format(kind: SourceKind, caps: &DeviceCaps) -> OutputSampleFormat {
    caps.formats
        .iter()
        .copied()
        .find(|&fmt| matches_source(kind, fmt))
        .or_else(|| caps.formats.first().copied())
        .unwrap_or_else(|| natural_format(kind))
}

/// Формат передаёт сэмплы источника без изменения разрядности и типа.
fn matches_source(kind: SourceKind, fmt: OutputSampleFormat) -> bool {
    match kind {
        SourceKind::Pcm { bits } => match bits.bits() {
            16 => fmt == OutputSampleFormat::S16Le,
            24 => matches!(fmt, OutputSampleFormat::S24Le | OutputSampleFormat::S24_3Le),
            32 => fmt == OutputSampleFormat::S32Le,
            _ => false,
        },
        SourceKind::FloatPcm { .. } => fmt == OutputSampleFormat::F32,
        SourceKind::Dsd { .. } => false,
    }
}

/// Естественный формат источника: начальный формат цели до шага 4 (виден в
/// `partial`, если план остановлен раньше) и формат при пустом `caps.formats`.
fn natural_format(kind: SourceKind) -> OutputSampleFormat {
    match kind {
        SourceKind::Pcm { bits } if bits.bits() <= 16 => OutputSampleFormat::S16Le,
        SourceKind::Pcm { bits } if bits.bits() <= 24 => OutputSampleFormat::S24Le,
        SourceKind::Pcm { .. } | SourceKind::Dsd { .. } => OutputSampleFormat::S32Le,
        SourceKind::FloatPcm { .. } => OutputSampleFormat::F32,
    }
}

/// Шаг 4 (§6.3): правила формата §6.5 (`ZeroPad`, `Truncate`, `IntToFloat`,
/// `Int32ToFloat`, `FloatNarrow`, `FloatToInt`, `FloatUnsupported`)
/// реализуются на шаге 8 этапа AM1.0-8.5. Пока формат цели выбирается
/// `device_sample_format`, преобразования не добавляются.
fn step_sample_format(draft: &mut PathPlan, caps: &DeviceCaps) {
    draft.target.sample = device_sample_format(draft.source.kind, caps);
}

// ---------------------------------------------------------------------------
// Шаг 7. Полезная нагрузка ring
// ---------------------------------------------------------------------------

/// Шаг 7 (§6.3, ОВС-10 п. 1): `ExactI32 { valid_bits }`, если нет ни
/// `Resample`, ни `Downmix`, ни `DsdToPcm` и источник не float-PCM; иначе `F32`.
fn step_ring(draft: &mut PathPlan) {
    let converted = draft.transforms.iter().any(|t| {
        matches!(
            t.kind,
            TransformKind::Resample { .. } | TransformKind::Downmix { .. } | TransformKind::DsdToPcm { .. }
        )
    }) || matches!(draft.route, Route::DsdToPcm { .. });
    draft.ring = match draft.source.kind {
        SourceKind::Pcm { bits } if !converted => RingPayload::ExactI32 { valid_bits: bits },
        SourceKind::Pcm { .. } | SourceKind::FloatPcm { .. } | SourceKind::Dsd { .. } => RingPayload::F32,
    };
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    fn sr(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    fn set(hz: &[u32]) -> SmallVec<[SampleRate; 16]> {
        hz.iter().map(|&h| sr(h)).collect()
    }

    /// §6.4: семейства 44,1/48 кГц и прочие; Range → стандартный ряд.
    #[test]
    fn family_classifies_rates() {
        assert_eq!(family(sr(44_100)), Family::F44k1);
        assert_eq!(family(sr(352_800)), Family::F44k1);
        assert_eq!(family(sr(48_000)), Family::F48k);
        assert_eq!(family(sr(768_000)), Family::F48k);
        assert_eq!(family(sr(12_000)), Family::Other);
        assert_eq!(family(sr(37_800)), Family::Other);
        let range = RateSet::Range { min: sr(48_000), max: sr(192_000) };
        assert_eq!(candidates(&range), set(&[48_000, 88_200, 96_000, 176_400, 192_000]));
    }

    /// ТЗ-27: SameFamily — сначала максимальная не выше, затем минимальная
    /// выше; без частот семейства — Nearest.
    #[test]
    fn same_family_prefers_lower_then_higher() {
        let s = set(&[44_100, 48_000, 96_000, 192_000]);
        assert_eq!(same_family(&s, sr(176_400)), Some(sr(44_100)));
        assert_eq!(same_family(&s, sr(88_200)), Some(sr(44_100)));
        let high = set(&[88_200, 176_400, 48_000]);
        assert_eq!(same_family(&high, sr(44_100)), Some(sr(88_200)));
        let only48 = set(&[48_000, 96_000]);
        assert_eq!(same_family(&only48, sr(88_200)), Some(sr(96_000)));
    }

    /// ТЗ-27: Nearest — при равном расстоянии большая частота.
    #[test]
    fn nearest_ties_pick_higher() {
        let s = set(&[40_000, 48_000]);
        assert_eq!(nearest(&s, sr(44_000)), Some(sr(48_000)));
        let t = set(&[44_100, 96_000]);
        assert_eq!(nearest(&t, sr(48_000)), Some(sr(44_100)));
        assert_eq!(nearest(&[], sr(48_000)), None);
    }

    /// ТЗ-27: NoDownsample — минимальная не ниже, иначе максимальная.
    #[test]
    fn no_downsample_falls_back_to_max() {
        let s = set(&[44_100, 48_000, 96_000]);
        assert_eq!(no_downsample(&s, sr(88_200)), Some(sr(96_000)));
        assert_eq!(no_downsample(&s, sr(48_000)), Some(sr(48_000)));
        assert_eq!(no_downsample(&s, sr(192_000)), Some(sr(96_000)));
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod plan_tests;
