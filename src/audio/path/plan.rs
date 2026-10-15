//! Планировщик пути сигнала: `plan(src, caps, policy)` (AM1.0 §6.3, §6.4, §6.5,
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
    BitDepth, ChannelLayout, ChannelPos, DopOutFormat, DsdRate, OutputSampleFormat, SampleRate, SourceFormat,
    SourceKind, StreamFormat,
};
use crate::audio::session::RingPayload;
use crate::audio::verdict::GainState;
use crate::settings::params::ParamId;
use crate::settings::playback::{Dither, DsdAboveDac, DsdFilter, ModeKind, PathPolicy, RateFallbackRule};

use super::transform::{Cause, Confidence, Transform, TransformKind, Truncate, ZeroPad};
use super::{Blocked, DopRoute, DsdChainStep, DsdChainView, DsdStepKind, DsdStepStatus, PathPlan, Route, StageId};

/// Стандартный ряд частот для `RateSet::Range` (§6.4), Гц.
const STANDARD_RATES_HZ: [u32; 10] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
];

/// Базовые частоты DSD→PCM (§6.9, ОВС-6): 88,2 и 176,4 кГц.
const RATE_88K2: SampleRate = const_rate(88_200);
const RATE_176K4: SampleRate = const_rate(176_400);

/// Разрядность нагрузки DoP в ring: 16 DSD-бит в битах 23..8 `ExactI32`,
/// младшие 8 бит — нули (ADR-12, §2.9). Решение шага AM1.0-8.5:
/// `valid_bits = 24`.
const DOP_PAYLOAD_BITS: BitDepth = match BitDepth::new(24) {
    Some(bits) => bits,
    None => panic!("24 бита — допустимая разрядность"),
};

/// Частота-константа; ноль отвергается при компиляции.
const fn const_rate(hz: u32) -> SampleRate {
    match SampleRate::new(hz) {
        Some(rate) => rate,
        None => panic!("частота-константа должна быть ненулевой"),
    }
}

/// Несовместимость шага: этап, на котором план остановлен, и причина (§6.3).
type StepError = (StageId, Incompatibility);

/// Строит план пути сигнала (§6.3, ТЗ-38).
///
/// Шаги выполняются строго в порядке таблицы §6.3. При первой
/// несовместимости возвращается `Blocked { at, reason, partial }`, где
/// `partial` — план с результатами уже выполненных шагов (ТЗ-80).
///
/// Текущее состояние каркаса (этап AM1.0-8.5):
/// - шаг 1 (маршрут, §6.9, ТЗ-23, ТЗ-40) реализован полностью: `Route::Pcm`,
///   `Dop` (только Exclusive, И-Т3) или `DsdToPcm`, цепочка `dsd_chain`; в
///   Строгом режиме без DoP — `DopUnsupported { dsd, max_dop }`. Для `Dop`
///   шаги 2–4 пропускаются: цель целиком задаёт маршрут (`dop_ok`
///   гарантирует каналы, частоту и формат);
/// - шаг 2 (каналы, §6.5, §6.13, ТЗ-54) реализован полностью: `MonoToStereo`,
///   `Downmix`, `ChannelPad` или `ChannelsUnsupported` в Строгом режиме;
/// - шаг 4 (формат сэмпла, §6.5, ТЗ-26, ТЗ-28, ТЗ-37, ТЗ-132) реализован
///   полностью: `ZeroPad`, `Truncate`, `IntToFloat`, `Int32ToFloat`,
///   `FloatNarrow`, `FloatToInt` или в Строгом режиме `TruncationForbidden` /
///   `FloatUnsupported`;
/// - шаг 3 (частота, §6.4) реализован полностью; для `DsdToPcm` частота
///   источника — `pcm_rate` маршрута;
/// - шаги 5–6 (усиление и дизеринг) выполняет не `plan()`, а
///   `with_gain(&plan, gain, policy)` (§6.3, ТЗ-8, ТЗ-60): громкость меняется
///   без переоткрытия, поэтому `plan()` возвращает план без `Volume`, `Mute`,
///   `SilenceGate` и `Dither`; движок применяет к нему `with_gain` со снимком
///   `GainState`;
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

/// Шаги §6.3 по порядку; шаги 5–6 — в `with_gain`, шаги 2–4 для `Dop` не
/// нужны (см. doc `plan`).
fn run_steps(draft: &mut PathPlan, caps: &DeviceCaps, policy: PathPolicy<'_>) -> Result<(), StepError> {
    step_route(draft, caps, policy)?;
    if !matches!(draft.route, Route::Dop(_)) {
        // DoP: каналы, частоту и формат уже выбрал шаг 1 (`dop_ok`, ADR-12).
        step_channels(draft, caps)?;
        step_rate(draft, caps, policy)?;
        step_sample_format(draft, caps)?;
    }
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

/// Шаг 1 (§6.3, §6.9, ТЗ-1, ТЗ-23, ТЗ-31, ТЗ-33, ТЗ-39, ТЗ-40, И-Т3, И-Т5).
///
/// PCM-источник → `Route::Pcm` без DSD-цепочки. DSD-источник проходит
/// цепочку Native → DoP → PCM; статусы шагов попадают в `dsd_chain` (ТЗ-40,
/// Native всегда виден и не пробуется — И-Т5):
/// - Совместимый: Native и DoP «отключено режимом», PCM выбран →
///   `DsdToPcm` с `Cause::ModeRule` (Shared не умеет DoP, И-Т3);
/// - Оптимальный: Native «не реализовано»; `Dop`, если `dop_ok`, иначе
///   `DsdToPcm`. Причина DSD→PCM: DSD выше возможностей устройства
///   (`max_dop` есть, но ниже `dsd`) → `PlayerSetting(DsdAboveDac)` (ТЗ-33,
///   `DsdAboveDac::ConvertToPcm`), иначе «устройство не поддерживает DoP» →
///   `DeviceLimit`;
/// - Строгий: Native «не реализовано», PCM «отключено режимом»; `Dop`, если
///   `dop_ok`, иначе `DopUnsupported { dsd, max_dop }` на этапе «Вывод».
///
/// `Dop` задаёт цель целиком: частота `dsd / 16`, формат `DopOutFormat`
/// (ADR-12), каналы источника (их гарантирует `dop_ok`); `DopPack` имеет
/// причину `ModeRule` (DoP закреплён режимом, строка «DSD: DoP» матрицы).
fn step_route(draft: &mut PathPlan, caps: &DeviceCaps, policy: PathPolicy<'_>) -> Result<(), StepError> {
    use DsdStepStatus::{Chosen, DisabledByMode, NotImplemented, NotTried, Unsupported};
    let SourceKind::Dsd { dsd } = draft.source.kind else {
        draft.route = Route::Pcm;
        draft.dsd_chain = None;
        return Ok(());
    };
    let channels = draft.source.layout.count().get();
    let dop = dop_route(dsd, channels, caps);
    match policy {
        PathPolicy::Compatible(opts) => {
            draft.dsd_chain = Some(dsd_chain(DisabledByMode, DisabledByMode, Chosen));
            route_dsd_to_pcm(draft, dsd, opts.dsd_filter, opts.dsd_gain_comp, caps, Cause::ModeRule);
            Ok(())
        }
        PathPolicy::Optimal(opts) => {
            if let Some((route, fmt)) = dop {
                draft.dsd_chain = Some(dsd_chain(NotImplemented, Chosen, NotTried));
                route_dop(draft, route, fmt);
                return Ok(());
            }
            draft.dsd_chain = Some(dsd_chain(NotImplemented, Unsupported, Chosen));
            let above_dac = max_dop(channels, caps).is_some_and(|max| max.bit_rate_hz() < dsd.bit_rate_hz());
            let cause = match (above_dac, opts.dsd_above_dac) {
                (true, DsdAboveDac::ConvertToPcm) => Cause::PlayerSetting(ParamId::DsdAboveDac),
                (false, _) => Cause::DeviceLimit,
            };
            route_dsd_to_pcm(draft, dsd, opts.dsd_filter, opts.dsd_gain_comp, caps, cause);
            Ok(())
        }
        PathPolicy::Strict(_) => {
            if let Some((route, fmt)) = dop {
                draft.dsd_chain = Some(dsd_chain(NotImplemented, Chosen, DisabledByMode));
                route_dop(draft, route, fmt);
                return Ok(());
            }
            draft.dsd_chain = Some(dsd_chain(NotImplemented, Unsupported, DisabledByMode));
            let max_dop = max_dop(channels, caps);
            Err((StageId::Output, Incompatibility::DopUnsupported { dsd, max_dop }))
        }
    }
}

/// Цепочка Native → DoP → PCM со статусами шагов (§6.9, ТЗ-40).
fn dsd_chain(native: DsdStepStatus, dop: DsdStepStatus, pcm: DsdStepStatus) -> DsdChainView {
    let step = |kind, status| DsdChainStep { kind, status };
    DsdChainView {
        steps: SmallVec::from_buf([
            step(DsdStepKind::Native, native),
            step(DsdStepKind::Dop, dop),
            step(DsdStepKind::Pcm, pcm),
        ]),
    }
}

/// Частота PCM-контейнера DoP: `dsd / 16` (DSD64 → 176,4 кГц, …, DSD512 →
/// 1411,2 кГц; §6.9).
fn dop_rate(dsd: DsdRate) -> Option<SampleRate> {
    SampleRate::new(dsd.bit_rate_hz() / 16)
}

/// `dop_ok` (§6.9, И-Т3, ADR-12): монопольный доступ, частота `dsd / 16`,
/// каналы источника и хотя бы один 24-битный контейнер DoP. Возвращает
/// формат DoP — первый поддерживаемый в порядке `S24_3LE`, `S24_LE`,
/// `S32_LE` (ADR-12); `None` — DoP невозможен. Решение шага AM1.0-8.5:
/// пустые `caps.channels` или `caps.formats` дают `None` (условие §6.9
/// буквально: `∋` и `∩ ≠ ∅`).
fn dop_format(dsd: DsdRate, channels: u8, caps: &DeviceCaps) -> Option<DopOutFormat> {
    if caps.access != AccessKind::Exclusive || !caps.channels.contains(&channels) {
        return None;
    }
    if !caps.rates.contains(dop_rate(dsd)?) {
        return None;
    }
    [DopOutFormat::S24_3Le, DopOutFormat::S24Le, DopOutFormat::S32Le]
        .into_iter()
        .find(|&f| caps.formats.contains(&OutputSampleFormat::from(f)))
}

/// Маршрут DoP и его формат, если выполняется `dop_ok` (§6.9); `DopRoute::new`
/// повторно проверяет монопольный доступ (И-Т3).
fn dop_route(dsd: DsdRate, channels: u8, caps: &DeviceCaps) -> Option<(DopRoute, DopOutFormat)> {
    let fmt = dop_format(dsd, channels, caps)?;
    let route = DopRoute::new(dsd, dop_rate(dsd)?, caps.access)?;
    Some((route, fmt))
}

/// `max_dop` (§6.9): наибольшая DSD-частота, для которой выполняется `dop_ok`.
fn max_dop(channels: u8, caps: &DeviceCaps) -> Option<DsdRate> {
    DsdRate::ALL.into_iter().rev().find(|&d| dop_format(d, channels, caps).is_some())
}

/// Маршрут `Dop` (§6.9, ADR-12): цель — частота контейнера и формат DoP,
/// каналы источника; преобразование `DopPack` без потерь.
fn route_dop(draft: &mut PathPlan, route: DopRoute, fmt: DopOutFormat) {
    draft.route = Route::Dop(route);
    draft.target.rate = route.pcm_rate();
    draft.target.sample = OutputSampleFormat::from(fmt);
    draft.target.channels = draft.source.layout.count();
    draft
        .transforms
        .push(assumed(TransformKind::DopPack { dsd: route.dsd() }, Cause::ModeRule));
}

/// Маршрут `DsdToPcm` (§6.9, ОВС-6, ОВС-6б, ТЗ-16, ТЗ-31, ТЗ-128): частота
/// `dsd_pcm_rate`, действующая граница фильтра `dsd_stop_hz`. Частоту и
/// формат выхода дальше выбирают шаги 3–4 от `pcm_rate` (нагрузка `F32`).
fn route_dsd_to_pcm(draft: &mut PathPlan, dsd: DsdRate, filter: DsdFilter, gain_comp: bool, caps: &DeviceCaps, cause: Cause) {
    let pcm_rate = dsd_pcm_rate(dsd, filter, caps);
    let stop_hz = dsd_stop_hz(filter, pcm_rate);
    draft.route = Route::DsdToPcm { dsd, pcm_rate, filter, stop_hz, gain_comp };
    draft.target.rate = pcm_rate;
    draft
        .transforms
        .push(assumed(TransformKind::DsdToPcm { dsd, rate: pcm_rate, filter, stop_hz, gain_comp }, cause));
}

/// Частота PCM для DSD→PCM (§6.9, ОВС-6, И-Р11): базовая 88,2 кГц для
/// DSD64 с фильтром не 50 кГц, иначе 176,4 кГц; первая поддерживаемая из
/// `base`, `base / 2`, `base / 4`; иначе наибольшая поддерживаемая частота
/// семейства 44,1 кГц ниже `base`, иначе наибольшая поддерживаемая. Решение
/// шага AM1.0-8.5: если кандидатов нет (пустой `Discrete` или `Range` без
/// стандартных частот) — `base`; её проверяет шаг 3.
///
/// Общая функция планировщика, декодера DSD→PCM и окна (И-Р11, ТЗ-16).
pub fn dsd_pcm_rate(dsd: DsdRate, filter: DsdFilter, caps: &DeviceCaps) -> SampleRate {
    let base = if dsd == DsdRate::Dsd64 && filter != DsdFilter::K50 { RATE_88K2 } else { RATE_176K4 };
    let halves = [base.hz(), base.hz() / 2, base.hz() / 4];
    if let Some(rate) = halves.into_iter().filter_map(SampleRate::new).find(|&r| caps.rates.contains(r)) {
        return rate;
    }
    let cands = candidates(&caps.rates);
    cands
        .iter()
        .copied()
        .filter(|&r| family(r) == Family::F44k1 && r < base)
        .max()
        .or_else(|| cands.iter().copied().max())
        .unwrap_or(base)
}

/// Действующая граница фильтра DSD→PCM: `min(filter, fs_out / 2)` (§6.9,
/// ТЗ-16, ТЗ-128, И-Р11).
pub fn dsd_stop_hz(filter: DsdFilter, fs_out: SampleRate) -> u32 {
    dsd_filter_hz(filter).min(fs_out.hz() / 2)
}

/// Выбранная граница фильтра DSD→PCM, Гц (ОВС-6): 24, 30 или 50 кГц.
fn dsd_filter_hz(filter: DsdFilter) -> u32 {
    match filter {
        DsdFilter::K24 => 24_000,
        DsdFilter::K30 => 30_000,
        DsdFilter::K50 => 50_000,
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
    let cause = limit_cause(caps);
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

/// Причина преобразования каналов и формата сэмпла (§6.5): ограничение
/// сервера (Shared) или устройства (Exclusive).
fn limit_cause(caps: &DeviceCaps) -> Cause {
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
///
/// Для `DsdToPcm` частота источника — `pcm_rate` маршрута (§6.9): она уже
/// выбрана из поддерживаемых, поэтому SRC возможен только по `fixed_rate`
/// Совместимого режима (решение шага AM1.0-8.5).
fn step_rate(draft: &mut PathPlan, caps: &DeviceCaps, policy: PathPolicy<'_>) -> Result<(), StepError> {
    let src_rate = match draft.route {
        Route::DsdToPcm { pcm_rate, .. } => pcm_rate,
        Route::Pcm | Route::Dop(_) => draft.source.rate,
    };
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
/// Шаг 4 (§6.3, §6.5, ТЗ-22, ТЗ-26, ТЗ-28, ТЗ-37, ТЗ-132, ОВС-1, ОВС-3, ОВС-4).
///
/// Правило выбирается по режиму (матрица ТЗ §5, строка «Формат сэмпла к точке
/// передачи»): Совместимый — Shared-правило (`F32`, если сервер его
/// предлагает), Оптимальный и Строгий — Exclusive-правило по целым форматам.
/// Причина преобразований — ограничение сервера или устройства
/// (`limit_cause`); дополнение нулями — `FormatContainer`.
///
/// В Строгом режиме float-источник → `FloatUnsupported` (ТЗ-132), а
/// недостаточная разрядность устройства → `TruncationForbidden` (ТЗ-37).
/// Если устройство форматов не сообщило, цель — естественный формат
/// источника без преобразований.
fn step_sample_format(draft: &mut PathPlan, caps: &DeviceCaps) -> Result<(), StepError> {
    if draft.mode == ModeKind::Strict && matches!(draft.source.kind, SourceKind::FloatPcm { .. }) {
        return Err((StageId::Output, Incompatibility::FloatUnsupported));
    }
    if caps.formats.is_empty() {
        draft.target.sample = natural_format(draft.source.kind);
        return Ok(());
    }
    let cause = limit_cause(caps);
    match draft.mode {
        ModeKind::Compatible => shared_format(draft, caps, cause),
        ModeKind::Optimal | ModeKind::Strict => exclusive_format(draft, caps, cause),
    }
}

/// Shared-правило (§6.5): `F32`, если сервер его предлагает, с
/// преобразованием из `float_conversion`. Иначе (ОВС-4, ТЗ-22) — правило
/// Exclusive-формата Оптимального режима по целым форматам сервера.
fn shared_format(draft: &mut PathPlan, caps: &DeviceCaps, cause: Cause) -> Result<(), StepError> {
    if !caps.formats.contains(&OutputSampleFormat::F32) {
        return exclusive_format(draft, caps, cause);
    }
    draft.target.sample = OutputSampleFormat::F32;
    if let Some(kind) = float_conversion(draft) {
        draft.transforms.push(assumed(kind, cause));
    }
    Ok(())
}

/// Преобразование при выходе `F32` (§6.5, ОВС-1): `IntToFloat { bits }` при
/// `valid_bits ≤ 24` (без потерь), `Int32ToFloat` при 32 битах (с потерями),
/// `FloatNarrow` для float64-источника (с потерями); при `F32`-нагрузке DSP и
/// для float32-источника — ничего.
fn float_conversion(draft: &PathPlan) -> Option<TransformKind> {
    match draft.source.kind {
        SourceKind::FloatPcm { double: true } => Some(TransformKind::FloatNarrow),
        SourceKind::FloatPcm { double: false } => None,
        _ if f32_payload(draft) => None,
        SourceKind::Pcm { bits } if bits.bits() <= 24 => Some(TransformKind::IntToFloat { bits }),
        SourceKind::Pcm { .. } => Some(TransformKind::Int32ToFloat),
        SourceKind::Dsd { .. } => None,
    }
}

/// Exclusive-правило (§6.5, ТЗ-26, ТЗ-28, ТЗ-37, ОВ-32, ОВС-1, ОВС-3).
///
/// Кандидаты — целые форматы устройства по возрастанию
/// `(field_bits, байт на сэмпл)`. План с `F32`-нагрузкой (SRC, downmix,
/// DSD→PCM, float-источник) получает наибольший формат; float-источник —
/// с `FloatToInt { bits }` (ОВС-1). Целая нагрузка с `valid_bits = need`:
/// первый формат с `field_bits ≥ need` (`ZeroPad`, если поле шире; 24 бита в
/// `S24_LE` — без преобразования, ОВ-32), иначе наибольший формат с
/// `Truncate` (Строгий — `TruncationForbidden`). Переквантование в целый
/// выход сопровождает дизеринг `with_gain` (§6.3, правило `requantizes`).
///
/// Если целых форматов нет (устройство сообщило только `F32`), используется
/// выход `F32` по Shared-правилу (`float_only_format`).
fn exclusive_format(draft: &mut PathPlan, caps: &DeviceCaps, cause: Cause) -> Result<(), StepError> {
    let cands = int_candidates(caps);
    let Some(&largest) = cands.last() else {
        return float_only_format(draft, cause);
    };
    if f32_payload(draft) {
        draft.target.sample = largest;
        if matches!(draft.source.kind, SourceKind::FloatPcm { .. }) {
            let kind = TransformKind::FloatToInt { bits: largest.field_bits() };
            draft.transforms.push(assumed(kind, cause));
        }
        return Ok(());
    }
    let SourceKind::Pcm { bits: need } = draft.source.kind else {
        // DSD сюда не доходит: для DoP шаг 4 пропускается (формат —
        // `DopOutFormat`, ADR-12), а DSD→PCM — это `F32`-нагрузка.
        draft.target.sample = largest;
        return Ok(());
    };
    if let Some(&fmt) = cands.iter().find(|f| f.field_bits() >= need.bits()) {
        draft.target.sample = fmt;
        if let Some(pad) = BitDepth::new(fmt.field_bits()).and_then(|to| ZeroPad::new(need, to)) {
            draft.transforms.push(assumed(TransformKind::ZeroPad(pad), Cause::FormatContainer));
        }
        return Ok(());
    }
    if draft.mode == ModeKind::Strict {
        return Err((StageId::Output, Incompatibility::TruncationForbidden { from: need, max: largest.field_bits() }));
    }
    draft.target.sample = largest;
    if let Some(cut) = BitDepth::new(largest.field_bits()).and_then(|to| Truncate::new(need, to)) {
        draft.transforms.push(assumed(TransformKind::Truncate(cut), cause));
    }
    Ok(())
}

/// Exclusive-правило без целых форматов (устройство сообщило только `F32`):
/// выход `F32` с преобразованием `float_conversion`. Строгий режим допускает
/// только преобразование без потерь (ТЗ-35): `Int32ToFloat` даёт
/// `TruncationForbidden` с максимумом 24 бита (мантисса float32).
fn float_only_format(draft: &mut PathPlan, cause: Cause) -> Result<(), StepError> {
    let kind = float_conversion(draft);
    if draft.mode == ModeKind::Strict {
        if let (Some(k), SourceKind::Pcm { bits }) = (&kind, draft.source.kind) {
            if !k.allowed_in(ModeKind::Strict) {
                let max = OutputSampleFormat::F32.field_bits();
                return Err((StageId::Output, Incompatibility::TruncationForbidden { from: bits, max }));
            }
        }
    }
    draft.target.sample = OutputSampleFormat::F32;
    if let Some(k) = kind {
        draft.transforms.push(assumed(k, cause));
    }
    Ok(())
}

/// Целые форматы устройства по возрастанию `(field_bits, байт на сэмпл)`
/// (§6.5): `S16_LE` < `S24_3LE` < `S24_LE` < `S32_LE`; `F32` не кандидат
/// (форматы Exclusive — только целые, ОВ-32).
fn int_candidates(caps: &DeviceCaps) -> SmallVec<[OutputSampleFormat; 5]> {
    let mut cands: SmallVec<[OutputSampleFormat; 5]> =
        caps.formats.iter().copied().filter(|&f| f != OutputSampleFormat::F32).collect();
    cands.sort_by_key(|&f| (f.field_bits(), sample_bytes(f)));
    cands.dedup();
    cands
}

/// Байт на сэмпл в точке передачи (§6.5).
fn sample_bytes(fmt: OutputSampleFormat) -> u8 {
    match fmt {
        OutputSampleFormat::S16Le => 2,
        OutputSampleFormat::S24_3Le => 3,
        OutputSampleFormat::S24Le | OutputSampleFormat::S32Le | OutputSampleFormat::F32 => 4,
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

/// Нагрузка ring — `F32` (§6.3 шаг 7, ОВС-10 п. 1): есть `Resample`,
/// `Downmix` или `DsdToPcm`, либо источник не целый PCM. Исключение — `Dop`:
/// нагрузка DoP целая (`ExactI32`, ADR-12).
fn f32_payload(draft: &PathPlan) -> bool {
    if matches!(draft.route, Route::Dop(_)) {
        return false;
    }
    let converted = draft.transforms.iter().any(|t| {
        matches!(
            t.kind,
            TransformKind::Resample { .. } | TransformKind::Downmix { .. } | TransformKind::DsdToPcm { .. }
        )
    }) || matches!(draft.route, Route::DsdToPcm { .. });
    converted || !matches!(draft.source.kind, SourceKind::Pcm { .. })
}

/// Правило переквантования (§6.3, ТЗ-8): переквантование есть, если
/// нагрузка ring `F32` при целом выходе (в том числе `FloatToInt`), есть
/// `Truncate` или есть `Volume(v)` с `v ≠ 100` при целом выходе. `ZeroPad`,
/// `MonoToStereo`, `ChannelPad`, `DopPack`, `IntToFloat` и выход `F32`
/// переквантования не дают. По нему `with_gain` решает, добавлять ли
/// `Dither` (шаг 6 §6.3), проверяя план уже с `Volume` шага 5; `plan`
/// дизеринг не добавляет.
pub fn requantizes(plan: &PathPlan) -> bool {
    let int_out = plan.target.sample != OutputSampleFormat::F32;
    let f32_to_int = int_out && matches!(plan.ring, RingPayload::F32);
    let lossy = plan.transforms.iter().any(|t| match t.kind {
        TransformKind::Truncate(_) => true,
        TransformKind::Volume(v) => int_out && v.get() != 100,
        _ => false,
    });
    f32_to_int || lossy
}

// ---------------------------------------------------------------------------
// Шаги 5–6. Усиление и дизеринг (with_gain)
// ---------------------------------------------------------------------------

/// Стадия усиления колбэка (ADR-23, ОВС-18, ОВС-19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GainStage {
    /// Ступени громкости нет, mute — подменой тишиной (Строгий, Оптимальный
    /// с фиксацией громкости, DoP; ТЗ-41).
    NoGain,
    /// Атомарная громкость и mute (Совместимый, Оптимальный без фиксации, PCM).
    AtomicGain,
}

/// Стадия усиления плана (ADR-23, ОВС-18, ОВС-19): `NoGain` для Строгого
/// режима, Оптимального с `volume_lock` и маршрута `Dop` (у DoP громкости нет,
/// ADR-12); иначе `AtomicGain`.
fn gain_stage(plan: &PathPlan, policy: PathPolicy<'_>) -> GainStage {
    if matches!(plan.route, Route::Dop(_)) {
        return GainStage::NoGain;
    }
    match policy {
        PathPolicy::Strict(_) => GainStage::NoGain,
        PathPolicy::Optimal(opts) if opts.volume_lock => GainStage::NoGain,
        PathPolicy::Optimal(_) | PathPolicy::Compatible(_) => GainStage::AtomicGain,
    }
}

/// Опция дизеринга режима (ТЗ-8, ОВ-7): у Строгого режима её нет — он не
/// переквантует (ТЗ-35, ТЗ-41), поэтому `Dither::Off`.
fn dither_opt(policy: PathPolicy<'_>) -> Dither {
    match policy {
        PathPolicy::Compatible(opts) => opts.dither,
        PathPolicy::Optimal(opts) => opts.dither,
        PathPolicy::Strict(_) => Dither::Off,
    }
}

/// Преобразования шагов 5–6, которые пересчитывает `with_gain` (§6.3).
fn is_gain_step(kind: &TransformKind) -> bool {
    matches!(
        kind,
        TransformKind::Volume(_) | TransformKind::Mute | TransformKind::SilenceGate | TransformKind::Dither(_)
    )
}

/// Шаги 5–6 планировщика (§6.3, ТЗ-8, ТЗ-41, ТЗ-60, ОВС-15, ОВС-18, ОВС-19,
/// ADR-23): новый план с усилением и дизерингом для снимка `gain`.
///
/// Громкость меняется без переоткрытия, поэтому шаги 5–6 выполняет не `plan`,
/// а эта функция: она меняет только `Volume`/`Mute`/`SilenceGate`/`Dither`, а
/// `route`, `target`, `ring` и преобразования шагов 1–4 сохраняет. Прежние
/// преобразования шагов 5–6 удаляются, поэтому повторный вызов идемпотентен.
///
/// Отличие от сигнатуры §6.3 `with_gain(&PathPlan, GainState)`: третий
/// параметр `policy`. Опции `dither` (`CompatibleOpts`/`OptimalOpts`) и
/// `volume_lock` (`OptimalOpts`) хранятся в настройках режима, а не в
/// `PathPlan` (решение шага AM1.0-8.5).
///
/// Шаг 5. Стадия `NoGain` (`gain_stage`): `Muted` → `SilenceGate`, иначе
/// ничего — ступени громкости нет (ТЗ-41). Стадия `AtomicGain`: `Muted` →
/// `Mute`; `Scaled(v)` с `v ≠ 100` → `Volume(v)`; `Scaled(100)`, `Unity`,
/// `NotApplicable` → ничего (единичное усиление побитово прозрачно, И-Р19).
///
/// Шаг 6. `Dither(Tpdf)`, если `requantizes` и опция режима — `Tpdf`.
/// `Dither::Off` в преобразования не вносится (решение шага AM1.0-8.5). При
/// mute дизеринг не добавляется: на выходе нули формата (ADR-03).
///
/// Причины (`Cause::PlayerSetting`): `Volume` — `ParamId::Volume`, `Mute` и
/// `SilenceGate` — `ParamId::Mute`, `Dither` — `ParamId::DitherKind`; все три
/// — пользовательские регуляторы, рекомендация укажет на них (§3.3, ТЗ-71).
pub fn with_gain(plan: &PathPlan, gain: GainState, policy: PathPolicy<'_>) -> PathPlan {
    let mut next = plan.clone();
    next.transforms.retain(|t| !is_gain_step(&t.kind));
    let stage = gain_stage(&next, policy);
    let gain_kind = match (stage, gain) {
        (GainStage::NoGain, GainState::Muted) => Some((TransformKind::SilenceGate, ParamId::Mute)),
        (GainStage::AtomicGain, GainState::Muted) => Some((TransformKind::Mute, ParamId::Mute)),
        (GainStage::AtomicGain, GainState::Scaled(v)) if v.get() != 100 => {
            Some((TransformKind::Volume(v), ParamId::Volume))
        }
        (GainStage::NoGain, _) | (GainStage::AtomicGain, _) => None,
    };
    if let Some((kind, param)) = gain_kind {
        next.transforms.push(assumed(kind, Cause::PlayerSetting(param)));
    }
    let muted = gain == GainState::Muted;
    if !muted && dither_opt(policy) == Dither::Tpdf && requantizes(&next) {
        next.transforms
            .push(assumed(TransformKind::Dither(Dither::Tpdf), Cause::PlayerSetting(ParamId::DitherKind)));
    }
    next
}

// ---------------------------------------------------------------------------
// Шаг 7. Полезная нагрузка ring
// ---------------------------------------------------------------------------

/// Шаг 7 (§6.3, ОВС-10 п. 1): `ExactI32 { valid_bits }`, если нет ни
/// `Resample`, ни `Downmix`, ни `DsdToPcm` и источник не float-PCM; иначе `F32`
/// (`f32_payload`). DoP — `ExactI32 { valid_bits: 24 }`: 16 бит DSD-нагрузки
/// в битах 23..8, маркер ставит колбэк (ADR-12, §2.9).
fn step_ring(draft: &mut PathPlan) {
    if matches!(draft.route, Route::Dop(_)) {
        draft.ring = RingPayload::ExactI32 { valid_bits: DOP_PAYLOAD_BITS };
        return;
    }
    draft.ring = match draft.source.kind {
        SourceKind::Pcm { bits } if !f32_payload(draft) => RingPayload::ExactI32 { valid_bits: bits },
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

    /// §6.5: кандидаты Exclusive — целые форматы по `(field_bits, байт)`,
    /// без `F32` и повторов.
    #[test]
    fn int_candidates_sorted_without_f32() {
        use OutputSampleFormat::{F32, S16Le, S24Le, S24_3Le, S32Le};
        let caps = DeviceCaps {
            api: crate::audio::backend::OutputApi::AlsaHw,
            access: AccessKind::Exclusive,
            name: "test".to_owned(),
            rates: RateSet::Discrete(set(&[48_000])),
            formats: SmallVec::from_slice(&[S32Le, F32, S24Le, S16Le, S24_3Le, S16Le]),
            channels: SmallVec::from_slice(&[2]),
        };
        assert_eq!(int_candidates(&caps).as_slice(), &[S16Le, S24_3Le, S24Le, S32Le]);
    }

    fn dsd_caps(access: AccessKind, hz: &[u32], formats: &[OutputSampleFormat]) -> DeviceCaps {
        DeviceCaps {
            api: crate::audio::backend::OutputApi::AlsaHw,
            access,
            name: "test".to_owned(),
            rates: RateSet::Discrete(set(hz)),
            formats: SmallVec::from_slice(formats),
            channels: SmallVec::from_slice(&[2]),
        }
    }

    /// §6.9, ОВС-6: базовая частота DSD→PCM и спуск `base / 2`, `base / 4`,
    /// затем семейство 44,1 кГц ниже `base`, затем наибольшая.
    #[test]
    fn dsd_pcm_rate_base_and_fallbacks() {
        let full = dsd_caps(AccessKind::Exclusive, &[44_100, 88_200, 176_400, 192_000], &[]);
        assert_eq!(dsd_pcm_rate(DsdRate::Dsd64, DsdFilter::K30, &full), sr(88_200));
        assert_eq!(dsd_pcm_rate(DsdRate::Dsd64, DsdFilter::K50, &full), sr(176_400));
        assert_eq!(dsd_pcm_rate(DsdRate::Dsd256, DsdFilter::K24, &full), sr(176_400));
        let max96 = dsd_caps(AccessKind::Exclusive, &[44_100, 48_000, 88_200, 96_000], &[]);
        assert_eq!(dsd_pcm_rate(DsdRate::Dsd512, DsdFilter::K30, &max96), sr(88_200));
        let odd = dsd_caps(AccessKind::Exclusive, &[22_050, 48_000], &[]);
        assert_eq!(dsd_pcm_rate(DsdRate::Dsd64, DsdFilter::K30, &odd), sr(22_050));
        let only48 = dsd_caps(AccessKind::Exclusive, &[48_000, 96_000], &[]);
        assert_eq!(dsd_pcm_rate(DsdRate::Dsd64, DsdFilter::K30, &only48), sr(96_000));
        let none = dsd_caps(AccessKind::Exclusive, &[], &[]);
        assert_eq!(dsd_pcm_rate(DsdRate::Dsd128, DsdFilter::K30, &none), sr(176_400));
    }

    /// §6.9, ТЗ-16: действующая граница — `min(filter, fs_out / 2)`.
    #[test]
    fn dsd_stop_hz_clamps_to_nyquist() {
        assert_eq!(dsd_stop_hz(DsdFilter::K30, sr(88_200)), 30_000);
        assert_eq!(dsd_stop_hz(DsdFilter::K50, sr(88_200)), 44_100);
        assert_eq!(dsd_stop_hz(DsdFilter::K30, sr(44_100)), 22_050);
        assert_eq!(dsd_stop_hz(DsdFilter::K24, sr(176_400)), 24_000);
    }

    /// §6.9, И-Т3, ADR-12: `dop_ok` только для Exclusive; формат — первый
    /// из `S24_3LE`, `S24_LE`, `S32_LE`; `max_dop` — наибольшая частота.
    #[test]
    fn dop_format_and_max_dop() {
        use OutputSampleFormat::{S16Le, S24Le, S24_3Le, S32Le};
        let hz = [176_400, 352_800, 705_600];
        let excl = dsd_caps(AccessKind::Exclusive, &hz, &[S16Le, S32Le, S24Le]);
        assert_eq!(dop_format(DsdRate::Dsd64, 2, &excl), Some(DopOutFormat::S24Le));
        assert_eq!(dop_format(DsdRate::Dsd64, 6, &excl), None);
        assert_eq!(dop_format(DsdRate::Dsd512, 2, &excl), None);
        assert_eq!(max_dop(2, &excl), Some(DsdRate::Dsd256));
        let packed = dsd_caps(AccessKind::Exclusive, &hz, &[S32Le, S24_3Le]);
        assert_eq!(dop_format(DsdRate::Dsd128, 2, &packed), Some(DopOutFormat::S24_3Le));
        let only16 = dsd_caps(AccessKind::Exclusive, &hz, &[S16Le]);
        assert_eq!(max_dop(2, &only16), None);
        let shared = dsd_caps(AccessKind::Shared, &hz, &[S24Le]);
        assert_eq!(max_dop(2, &shared), None);
        assert!(dop_route(DsdRate::Dsd64, 2, &shared).is_none());
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod plan_tests;
