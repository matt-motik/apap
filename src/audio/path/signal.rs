//! Снимок открытого потока `SignalPath` и динамическая часть пути сигнала
//! (AM1.0 §3.1, §3.2, §3.5, ADR-05, ТЗ-68…ТЗ-78).
//!
//! `SignalPath` строится из `PathPlan` и фактов открытия `OpenedFacts` чистой
//! функцией и далее неизменяем (передаётся как `Arc`, ADR-05). Величины,
//! меняющиеся во время воспроизведения, — в `DynamicPathState` (§3.5).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use crate::audio::backend::DriverFormat;
use crate::audio::test_run::TestBadge;
use crate::settings::playback::ModeKind;

use super::transform::{Lossiness, Transform};
use super::{
    DecoderStage, DeviceStage, DsdChainView, OpenedFacts, OsStage, OutputStage, PathPlan, ProcessingStage,
    ReservationView, Route, SourceStage, StageId, StageStatus,
};

// ---------------------------------------------------------------------------
// §3.1. Снимок открытого потока
// ---------------------------------------------------------------------------

/// Снимок открытого потока (§3.1, ADR-05; заменяет `StreamDesc`). Неизменяем.
///
/// Решение шага AM1.0-8.5 (дополнение к §3.1/§3.2): снимок хранит также
/// маршрут `route` и DSD-цепочку `dsd_chain` из плана — они нужны окну для
/// показа DSD-цепочки (ТЗ-23, ТЗ-40). В `ProcessingStage` (§3.2, `path/mod.rs`)
/// они не добавлены, чтобы не менять тип этапа из спецификации. Полезная
/// нагрузка кольца в снимок не входит: движок берёт её из `PathPlan`.
#[derive(Clone, PartialEq, Debug)]
pub struct SignalPath {
    /// Поколение потока: растёт при каждом открытии (сброс счётчиков, ТЗ-68).
    pub generation: u64,
    pub mode: ModeKind,
    pub source: SourceStage,
    pub decoder: DecoderStage,
    pub processing: ProcessingStage,
    pub output: OutputStage,
    pub os: OsStage,
    pub device: DeviceStage,
    /// Маршрут плана: PCM, DoP или DSD→PCM (ТЗ-23, ТЗ-40). Дополнение к §3.1.
    pub route: Route,
    /// Шаги DSD-цепочки со статусами (ТЗ-23, ТЗ-40). Дополнение к §3.1.
    pub dsd_chain: Option<DsdChainView>,
}

impl SignalPath {
    /// План + факты открытия (§3.1, ADR-05). Чистое построение без
    /// перепланирования: если драйвер принял другой формат, вызывающий
    /// перепланирует (Оптимальный) или пропускает трек (Строгий) — ADR-07.
    ///
    /// `DeviceStage::accepted` (ТЗ-78) — формат, принятый драйвером, если он
    /// известен: `DriverFormat::Confirmed(f)` → `Some(f)`; при
    /// `DriverFormat::Unreadable` принятый формат не подтверждён, поэтому
    /// `None` (запрошенный формат остаётся в `OutputStage::requested`, ОВ-14).
    pub fn from_opened(plan: PathPlan, facts: OpenedFacts, generation: u64) -> Self {
        let PathPlan { mode, source, route, transforms, target, ring: _, dsd_chain, dac } = plan;
        let OpenedFacts { access, api, device_name, driver, reservation, os, device_buffer } = facts;
        let accepted = match driver {
            DriverFormat::Confirmed(f) => Some(f),
            DriverFormat::Unreadable { .. } => None,
        };
        // ТЗ-75: декодирование без потерь, если источник не lossy (ОВ-36).
        let decoder = DecoderStage { lossless: !source.lossy };
        SignalPath {
            generation,
            mode,
            source: SourceStage { fmt: source },
            decoder,
            processing: ProcessingStage { transforms },
            output: OutputStage {
                api,
                access,
                device_name,
                requested: target,
                driver,
                // ОВ-35: `hw:` без резервирования — предупреждение на этапе «Вывод».
                reservation_warning: matches!(reservation, ReservationView::UnavailableNoServer),
                device_buffer,
            },
            os,
            device: DeviceStage { accepted, dac },
            route,
            dsd_chain,
        }
    }

    /// Все шесть этапов в фиксированном порядке (ТЗ-70): Источник, Декодер,
    /// Обработка, Вывод, Уровень ОС, Устройство.
    pub fn stages(&self) -> [StageView<'_>; 6] {
        [
            StageView::Source(&self.source),
            StageView::Decoder(&self.decoder),
            StageView::Processing(&self.processing),
            StageView::Output(&self.output),
            StageView::OsLevel(&self.os),
            StageView::Device(&self.device),
        ]
    }

    /// Все преобразования с потерями по всем этапам (ТЗ-72). Преобразования
    /// есть только у этапа «Обработка»; округление lossy-источника до 24 бит
    /// отдельным преобразованием не показывается (ТЗ-75, ОВ-36).
    pub fn lossy(&self) -> impl Iterator<Item = &Transform> {
        self.processing
            .transforms
            .iter()
            .filter(|t| matches!(t.kind.lossiness(), Lossiness::Lossy))
    }
}

// ---------------------------------------------------------------------------
// §3.2. Представление этапа
// ---------------------------------------------------------------------------

/// Ссылка на этап снимка для единообразного обхода (§3.2, ТЗ-70).
///
/// Спецификация тип не определяет; решение шага AM1.0-8.5: вариант на каждый
/// `StageId`, статус вычисляется, а не хранится (§3.2).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum StageView<'a> {
    Source(&'a SourceStage),
    Decoder(&'a DecoderStage),
    Processing(&'a ProcessingStage),
    Output(&'a OutputStage),
    OsLevel(&'a OsStage),
    Device(&'a DeviceStage),
}

impl StageView<'_> {
    /// Идентификатор этапа (§3.2).
    pub fn id(&self) -> StageId {
        match self {
            StageView::Source(_) => StageId::Source,
            StageView::Decoder(_) => StageId::Decoder,
            StageView::Processing(_) => StageId::Processing,
            StageView::Output(_) => StageId::Output,
            StageView::OsLevel(_) => StageId::OsLevel,
            StageView::Device(_) => StageId::Device,
        }
    }

    /// Статус этапа по правилам §3.2:
    /// - Источник — `Unchanged` (ТЗ-74: показываются факты файла);
    /// - Декодер — `Lossy` для lossy-источника (ТЗ-75, ОВ-36), иначе
    ///   `Unchanged`; MD5 «не совпал» статус не меняет (ОВ-15);
    /// - Обработка — максимум по `Transform::lossiness()`, пусто → `Unchanged`;
    /// - Вывод — `Unknown` при `DriverFormat::Unreadable` (ОВ-14), иначе `Unchanged`;
    /// - Уровень ОС — `Unchanged` при `NotInvolved`, `Unknown` при `Unknown`;
    ///   `Known` тоже `Unknown`: системный микшер плеером не контролируется
    ///   (ТЗ-77), и даже совпадение частоты не исключает преобразования
    ///   (громкость, формат микшера), поэтому статус лучше «Без изменений»
    ///   не завышается (ТЗ-52). Совпадение `rate` с частотой потока влияет
    ///   только на рекомендации (§6, «Настройки ОС»);
    /// - Устройство — всегда `Unknown`, граница гарантии (ТЗ-78).
    pub fn status(&self) -> StageStatus {
        match self {
            StageView::Source(_) => StageStatus::Unchanged,
            StageView::Decoder(d) => {
                if d.lossless {
                    StageStatus::Unchanged
                } else {
                    StageStatus::Lossy
                }
            }
            StageView::Processing(p) => p.status(),
            StageView::Output(o) => o.status(),
            StageView::OsLevel(os) => os.status(),
            StageView::Device(d) => d.status(),
        }
    }
}

// ---------------------------------------------------------------------------
// §3.5. Динамическая часть
// ---------------------------------------------------------------------------

/// Меняется во время воспроизведения; приходит событием `DynamicsChanged`
/// только при изменении (§3.5, ADR-05).
#[derive(Clone, PartialEq, Debug, Default)]
pub struct DynamicPathState {
    /// ТЗ-68; сброс при новом потоке.
    pub underruns: u32,
    /// ТЗ-68; только Exclusive, отдельно от underruns.
    pub xruns: u32,
    /// ОВС-8: заметка этапа «Вывод».
    pub rt_priority: RtPriorityState,
    /// ОВС-11: заметка этапа «Декодер» («ограничено N сэмплов»).
    pub lossy_clipped: u64,
    /// ОВС-6б: заметка этапа «Обработка».
    pub dsd_clipped: u64,
    /// ТЗ-75: ошибки декодирования за время воспроизведения трека.
    pub decode_errors: u32,
    /// ТЗ-69: результат проверки MD5.
    pub md5: Md5Status,
    /// Кнопка-бейдж «Тест» (§3.5, §2.7, ТЗ-61). `None` — состояние ещё не
    /// вычислено движком (нет потока или хранилище результатов не прочитано).
    pub test: Option<TestBadge>,
}

/// Результат проверки MD5 потока FLAC (§3.5, ТЗ-69, ОВ-15).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Md5Status {
    /// Трек ещё не доигран до конца.
    #[default]
    Pending,
    /// MD5 совпал.
    Matched,
    /// MD5 не совпал: отметка на этапе «Декодер», статус не меняется (ОВ-15).
    Mismatched,
    /// В файле нет MD5.
    Absent,
    /// Проверка не выполнялась.
    NotChecked(NotCheckedWhy),
}

/// Почему MD5 не проверялся (§3.5, ТЗ-69).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotCheckedWhy {
    /// Была перемотка: поток прочитан не целиком.
    Seeked,
    /// Источник не FLAC.
    NotFlac,
}

/// Итог запроса RT-приоритета (§3.5, §6.8, ОВС-8). Shared — `NotRequested`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RtPriorityState {
    /// Не запрашивался (Shared).
    #[default]
    NotRequested,
    /// Запрос в процессе.
    Pending,
    /// Выдан с указанным приоритетом.
    Granted(u32),
    /// Отказано.
    Denied,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use std::num::NonZeroU8;

    use smallvec::SmallVec;

    use super::super::transform::{Cause, Confidence, TransformKind};
    use super::super::DacCaps;
    use super::*;
    use crate::audio::backend::{AccessKind, DeviceBufferActual, OutputApi, RateSet};
    use crate::audio::format::{
        BitDepth, ChannelLayout, Codec, Container, OutputSampleFormat, SampleRate, SourceFormat, SourceKind,
        StreamFormat,
    };
    use crate::audio::session::RingPayload;

    fn sr(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    fn fmt(hz: u32) -> StreamFormat {
        StreamFormat { rate: sr(hz), sample: OutputSampleFormat::S32Le, channels: NonZeroU8::new(2).unwrap() }
    }

    fn tr(kind: TransformKind) -> Transform {
        Transform { kind, cause: Cause::ModeRule, confidence: Confidence::Assumed }
    }

    fn src(lossy: bool) -> SourceFormat {
        SourceFormat {
            container: Container::Flac,
            codec: Codec::Flac,
            lossy,
            rate: sr(44_100),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::Pcm { bits: BitDepth::new(16).unwrap() },
        }
    }

    fn plan_with(transforms: &[TransformKind]) -> PathPlan {
        PathPlan {
            mode: ModeKind::Optimal,
            source: src(false),
            route: Route::Pcm,
            transforms: transforms.iter().cloned().map(tr).collect(),
            target: fmt(44_100),
            ring: RingPayload::ExactI32 { valid_bits: BitDepth::new(16).unwrap() },
            dsd_chain: None,
            dac: DacCaps::default(),
        }
    }

    fn facts(driver: DriverFormat) -> OpenedFacts {
        OpenedFacts {
            access: AccessKind::Exclusive,
            api: OutputApi::AlsaHw,
            device_name: "hw:0".to_owned(),
            driver,
            reservation: ReservationView::UnavailableNoServer,
            os: OsStage::NotInvolved,
            device_buffer: Some(DeviceBufferActual { period_frames: 512, buffer_frames: 2048 }),
        }
    }

    /// ТЗ-70: шесть этапов в фиксированном порядке.
    #[test]
    fn stages_order_is_fixed() {
        let p = SignalPath::from_opened(plan_with(&[]), facts(DriverFormat::Confirmed(fmt(44_100))), 1);
        let ids = p.stages().map(|s| s.id());
        use StageId::*;
        assert_eq!(ids, [Source, Decoder, Processing, Output, OsLevel, Device]);
        let st = p.stages().map(|s| s.status());
        use StageStatus::*;
        assert_eq!(st, [Unchanged, Unchanged, Unchanged, Unchanged, Unchanged, Unknown]);
    }

    /// §3.2: статус «Обработки» — максимум по lossiness(); lossy-источник → Decoder Lossy.
    #[test]
    fn processing_status_is_max_lossiness() {
        let ok = facts(DriverFormat::Confirmed(fmt(44_100)));
        let empty = SignalPath::from_opened(plan_with(&[]), ok.clone(), 1);
        assert_eq!(empty.stages()[2].status(), StageStatus::Unchanged);
        let lossless = SignalPath::from_opened(plan_with(&[TransformKind::MonoToStereo]), ok.clone(), 1);
        assert_eq!(lossless.stages()[2].status(), StageStatus::Lossless);
        let mixed =
            SignalPath::from_opened(plan_with(&[TransformKind::MonoToStereo, TransformKind::Mute]), ok.clone(), 1);
        assert_eq!(mixed.stages()[2].status(), StageStatus::Lossy);

        let mut lossy_src = plan_with(&[]);
        lossy_src.source = src(true);
        let p = SignalPath::from_opened(lossy_src, ok, 1);
        assert!(!p.decoder.lossless);
        assert_eq!(p.stages()[1].status(), StageStatus::Lossy);
    }

    /// §3.1: поля плана и фактов переносятся в этапы, generation сохраняется.
    #[test]
    fn from_opened_moves_plan_and_facts() {
        let plan = plan_with(&[TransformKind::Mute]);
        let expected_src = plan.source.clone();
        let expected_tr = plan.transforms.clone();
        let p = SignalPath::from_opened(plan, facts(DriverFormat::Confirmed(fmt(48_000))), 7);
        assert_eq!(p.generation, 7);
        assert_eq!(p.mode, ModeKind::Optimal);
        assert_eq!(p.source.fmt, expected_src);
        assert!(p.decoder.lossless);
        assert_eq!(p.processing.transforms, expected_tr);
        assert_eq!(p.output.api, OutputApi::AlsaHw);
        assert_eq!(p.output.access, AccessKind::Exclusive);
        assert_eq!(p.output.device_name, "hw:0");
        assert_eq!(p.output.requested, fmt(44_100));
        assert_eq!(p.output.driver, DriverFormat::Confirmed(fmt(48_000)));
        assert!(p.output.reservation_warning);
        assert_eq!(p.output.device_buffer, Some(DeviceBufferActual { period_frames: 512, buffer_frames: 2048 }));
        assert_eq!(p.os, OsStage::NotInvolved);
        assert_eq!(p.device.accepted, Some(fmt(48_000)));
        assert_eq!(p.route, Route::Pcm);
        assert_eq!(p.dsd_chain, None);

        let mut held = facts(DriverFormat::Confirmed(fmt(44_100)));
        held.reservation = ReservationView::Held;
        assert!(!SignalPath::from_opened(plan_with(&[]), held, 1).output.reservation_warning);
    }

    /// §3.1, AM1.0-B29: `dac` плана переносится в `DeviceStage` без изменений.
    #[test]
    fn from_opened_carries_dac_caps() {
        let mut plan = plan_with(&[]);
        plan.dac = DacCaps {
            rates: RateSet::Discrete(SmallVec::from_slice(&[sr(44_100), sr(48_000)])),
            max_bits: 24,
            float_only: false,
            channels: SmallVec::from_slice(&[2]),
            max_dop: None,
        };
        let expected = plan.dac.clone();
        let p = SignalPath::from_opened(plan, facts(DriverFormat::Confirmed(fmt(44_100))), 1);
        assert_eq!(p.device.dac, expected);
    }

    /// ТЗ-72: lossy() перечисляет только преобразования с потерями, в порядке плана.
    #[test]
    fn lossy_lists_only_lossy_transforms() {
        let p = SignalPath::from_opened(
            plan_with(&[TransformKind::MonoToStereo, TransformKind::Mute]),
            facts(DriverFormat::Confirmed(fmt(44_100))),
            1,
        );
        let lossy: SmallVec<[&Transform; 2]> = p.lossy().collect();
        assert_eq!(lossy.len(), 1);
        assert_eq!(lossy[0].kind, TransformKind::Mute);
        let none = SignalPath::from_opened(
            plan_with(&[TransformKind::MonoToStereo]),
            facts(DriverFormat::Confirmed(fmt(44_100))),
            1,
        );
        assert_eq!(none.lossy().count(), 0);
    }

    /// §3.2, ОВ-14: нечитаемые параметры драйвера → «Вывод» Unknown, accepted = None.
    #[test]
    fn output_unknown_when_driver_unreadable() {
        let p = SignalPath::from_opened(plan_with(&[]), facts(DriverFormat::Unreadable { requested: fmt(44_100) }), 1);
        assert_eq!(p.stages()[3].status(), StageStatus::Unknown);
        assert_eq!(p.device.accepted, None);
        assert_eq!(p.stages()[5].status(), StageStatus::Unknown);
    }

    /// §3.5, ТЗ-61: новый поток — без бейджа «Теста»; бейдж входит в сравнение
    /// для `DynamicsChanged` (событие только при изменении, ADR-05).
    #[test]
    fn dynamic_state_test_badge() {
        use crate::audio::test_run::{IncompleteWhy, TestBadge};
        let d = DynamicPathState::default();
        assert_eq!(d.test, None);
        let with = DynamicPathState { test: Some(TestBadge::Unknown(Some(IncompleteWhy::Seek))), ..d.clone() };
        assert_ne!(with, d);
    }
}
