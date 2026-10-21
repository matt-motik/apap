//! Рекомендации окна «Путь сигнала»: что изменить в плеере, в ОС или в
//! оборудовании, чтобы получить bit-perfect (AM1.0 §3.4, §6.25, ТЗ-68,
//! ТЗ-80…ТЗ-84, ТЗ-129, ТЗ-131, И-Т10).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use smallvec::SmallVec;

use crate::audio::error::Incompatibility;
use crate::audio::format::{BitDepth, DsdRate, SampleRate};
use crate::settings::params::{availability, Availability, ParamId, SettingsAnchor, PARAMS};
use crate::settings::playback::{buffers_allowed, DeviceBuffer, ModeKind, ModeSettings};

use super::signal::{DynamicPathState, RtPriorityState, SignalPath};
use super::transform::{Cause, Lossiness, TransformKind};
use super::{Blocked, OsServer, OsStage};

/// Рекомендация окна «Путь сигнала» (§3.4, ТЗ-80…ТЗ-82).
#[derive(Clone, PartialEq, Debug)]
pub enum Recommendation {
    /// «Настройки плеера» с переходом к настройке (ТЗ-81).
    PlayerSetting(PlayerSettingRec),
    /// «Настройки ОС» с пояснением.
    Os { topic: OsTopic },
    /// «Оборудование»: какие характеристики нужны для bit-perfect на этом материале.
    Hardware { need: HwRequirement },
    /// Bit-perfect достижим только в другом режиме (ТЗ-82).
    SwitchMode { to: ModeKind, why: SwitchWhy },
}

/// Рекомендация «Настройки плеера» (§3.4, ТЗ-81). Непубличные поля: строится
/// только через `new`, который проверяет доступность параметра (ТЗ-82, И-Т10).
///
/// Решение шага AM1.0-8.5: `anchor` — `Option`, потому что якоря диалога в
/// `PARAMS` пока не заполнены (`ParamDescriptor::settings_anchor`); кнопка
/// «перейти к настройке» без якоря открывает диалог на вкладке режима.
#[derive(Clone, PartialEq, Debug)]
pub struct PlayerSettingRec {
    param: ParamId,
    anchor: Option<SettingsAnchor>,
    action: RecAction,
}

impl PlayerSettingRec {
    /// `None`, если параметр не `Configurable` в режиме `mode` по `PARAMS` (И-Т10).
    pub fn new(param: ParamId, mode: ModeKind, action: RecAction) -> Option<Self> {
        if availability(param, mode) != Availability::Configurable {
            return None;
        }
        let anchor = PARAMS.iter().find(|d| d.id == param).and_then(|d| d.settings_anchor);
        Some(PlayerSettingRec { param, anchor, action })
    }

    /// Параметр, который предлагается изменить.
    pub fn param(&self) -> ParamId {
        self.param
    }

    /// Вкладка и элемент диалога настроек (ТЗ-81).
    pub fn anchor(&self) -> Option<SettingsAnchor> {
        self.anchor
    }

    /// Что сделать с параметром.
    pub fn action(&self) -> RecAction {
        self.action
    }
}

/// Действие с параметром рекомендации (§6.25). Спецификация тип не
/// определяет; решение шага AM1.0-8.5.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecAction {
    /// Громкость → 100 %.
    SetVolumeFull,
    /// Выключить mute.
    Unmute,
    /// Выключить (фиксированная частота, компенсация +6 дБ DSD→PCM).
    TurnOff,
    /// Включить («ЦАП понимает DoP», ТЗ-145).
    TurnOn,
    /// Выбрать фильтр SRC «Качество».
    ChooseQualityFilter,
    /// Увеличить (буфер плеера, буфер устройства).
    Increase,
}

/// Тема рекомендации «Настройки ОС» (§3.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OsTopic {
    PipeWireAllowedRates,
    PipeWireForceRate,
    WindowsSharedFormat,
    WindowsExclusivePermission,
    /// Xrun при неполученном RT-приоритете: rtkit, группа `audio`, RLIMIT_RTPRIO (ТЗ-68).
    RealtimePriority,
}

/// Характеристика оборудования, нужная для bit-perfect (§3.4, ТЗ-80).
#[derive(Clone, PartialEq, Debug)]
pub enum HwRequirement {
    Rate(SampleRate),
    BitDepth(BitDepth),
    Channels(u8),
    DopRate(DsdRate),
}

/// Почему предлагается другой режим (§6.25, ТЗ-82). Спецификация тип не
/// определяет; решение шага AM1.0-8.5: подпись рекомендации.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SwitchWhy {
    /// «ресемплинг» — Оптимальный сыграет с SRC.
    Resample,
    /// «усечение разрядности».
    Truncate,
    /// «сведение каналов».
    Downmix,
    /// «DSD→PCM».
    DsdToPcm,
    /// «Exclusive» — обход системного микшера.
    Exclusive,
}

/// Предмет окна и рекомендаций: открытый поток, пропущенный трек или
/// отсутствие потока (§3.4, §3.6).
#[derive(Clone, Copy, Debug)]
pub enum PathSubject<'a> {
    Opened(&'a SignalPath),
    Skipped(&'a Blocked),
    NoStream,
}

/// Рекомендации из снимка или плана пропуска и динамики (§6.25). Чистая
/// функция: правила проходятся в фиксированном порядке таблицы §6.25,
/// повторы отбрасываются.
///
/// Решение шага AM1.0-8.5: для `NoStream` рекомендаций нет — режим потока
/// неизвестен, а счётчики динамики сбрасываются с новым потоком (ТЗ-68).
pub fn recommend(
    subject: PathSubject<'_>,
    dyn_state: &DynamicPathState,
    settings: &ModeSettings,
) -> SmallVec<[Recommendation; 6]> {
    let mut out = Recs::default();
    let mode = match subject {
        PathSubject::Opened(path) => {
            transform_rules(path, &mut out);
            os_rules(path, &mut out);
            path.mode
        }
        PathSubject::Skipped(blocked) => {
            blocked_rules(blocked, &mut out);
            blocked.partial.mode
        }
        PathSubject::NoStream => return out.0,
    };
    dynamic_rules(mode, dyn_state, settings, &mut out);
    out.0
}

/// Накопитель без повторов (порядок первого появления сохраняется).
#[derive(Default)]
struct Recs(SmallVec<[Recommendation; 6]>);

impl Recs {
    fn push(&mut self, r: Recommendation) {
        if !self.0.contains(&r) {
            self.0.push(r);
        }
    }

    /// Только если параметр `Configurable` в режиме (И-Т10).
    fn player(&mut self, param: ParamId, mode: ModeKind, action: RecAction) {
        if let Some(rec) = PlayerSettingRec::new(param, mode, action) {
            self.push(Recommendation::PlayerSetting(rec));
        }
    }

    fn hw(&mut self, need: HwRequirement) {
        self.push(Recommendation::Hardware { need });
    }

    fn switch(&mut self, to: ModeKind, why: SwitchWhy) {
        self.push(Recommendation::SwitchMode { to, why });
    }

    /// В Строгом режиме оборудование дополняется переходом в Оптимальный (ТЗ-80).
    fn hw_or_optimal(&mut self, mode: ModeKind, need: HwRequirement, why: SwitchWhy) {
        self.hw(need);
        if mode == ModeKind::Strict {
            self.switch(ModeKind::Optimal, why);
        }
    }
}

/// Действие для параметра, управляющего преобразованием (§6.25); `None` —
/// у параметра нет действия, убирающего потерю.
fn action_for(param: ParamId) -> Option<RecAction> {
    match param {
        ParamId::Volume => Some(RecAction::SetVolumeFull),
        ParamId::Mute => Some(RecAction::Unmute),
        ParamId::FixedRate => Some(RecAction::TurnOff),
        ParamId::SrcFilter => Some(RecAction::ChooseQualityFilter),
        ParamId::DeviceDop => Some(RecAction::TurnOn),
        _ => None,
    }
}

/// Правила по преобразованиям снимка (§6.25, ТЗ-80, ТЗ-81). Рекомендации
/// плеера — только для преобразований с потерями.
fn transform_rules(path: &SignalPath, out: &mut Recs) {
    let mode = path.mode;
    for t in &path.processing.transforms {
        if t.kind.lossiness() == Lossiness::Lossy {
            // Причина «настройка плеера» указывает на параметр точнее, чем вид.
            let param = match t.cause {
                Cause::PlayerSetting(p) => Some(p),
                _ => t.kind.controlling_param(),
            };
            if let Some(p) = param {
                if let Some(action) = action_for(p) {
                    out.player(p, mode, action);
                }
            }
        }
        match &t.kind {
            TransformKind::Resample { from, .. } if t.cause == Cause::DeviceLimit => {
                out.hw_or_optimal(mode, HwRequirement::Rate(*from), SwitchWhy::Resample);
            }
            TransformKind::Truncate(tr) => {
                out.hw_or_optimal(mode, HwRequirement::BitDepth(tr.from()), SwitchWhy::Truncate);
            }
            TransformKind::Downmix { from, .. } => {
                out.hw_or_optimal(mode, HwRequirement::Channels(from.count().get()), SwitchWhy::Downmix);
            }
            // DoP выключен для устройства (ТЗ-145, ADR-25) — рекомендация уже
            // дана выше («Настройки плеера»: `DeviceDop`); «Оборудование» —
            // только для DSD→PCM по другой причине (§6.25).
            TransformKind::DsdToPcm { dsd, .. }
                if mode == ModeKind::Optimal && t.cause != Cause::PlayerSetting(ParamId::DeviceDop) =>
            {
                out.hw(HwRequirement::DopRate(*dsd));
            }
            _ => {}
        }
    }
}

/// Правила уровня ОС для Совместимого режима (§6.25, ТЗ-82).
fn os_rules(path: &SignalPath, out: &mut Recs) {
    if path.mode != ModeKind::Compatible {
        return;
    }
    match &path.os {
        OsStage::Known { server, rate: Some(rate), .. } if *rate != path.output.requested.rate => {
            let topic = match server {
                OsServer::WasapiMixer => OsTopic::WindowsSharedFormat,
                OsServer::PipeWire | OsServer::PulseAudio => OsTopic::PipeWireAllowedRates,
            };
            out.push(Recommendation::Os { topic });
            out.switch(ModeKind::Optimal, SwitchWhy::Exclusive);
        }
        OsStage::Unknown => out.switch(ModeKind::Optimal, SwitchWhy::Exclusive),
        OsStage::Known { .. } | OsStage::NotInvolved => {}
    }
}

/// Правила для пропущенного трека (§6.25, ТЗ-80).
fn blocked_rules(blocked: &Blocked, out: &mut Recs) {
    let mode = blocked.partial.mode;
    match &blocked.reason {
        Incompatibility::RateUnsupported { rate, .. } => {
            out.hw_or_optimal(mode, HwRequirement::Rate(*rate), SwitchWhy::Resample);
        }
        Incompatibility::TruncationForbidden { from, .. } => {
            out.hw_or_optimal(mode, HwRequirement::BitDepth(*from), SwitchWhy::Truncate);
        }
        Incompatibility::ChannelsUnsupported { src, .. } => {
            out.hw_or_optimal(mode, HwRequirement::Channels(*src), SwitchWhy::Downmix);
        }
        Incompatibility::DopUnsupported { dsd, .. } => {
            out.hw(HwRequirement::DopRate(*dsd));
            out.switch(ModeKind::Optimal, SwitchWhy::DsdToPcm);
        }
        Incompatibility::DopDisabled { .. } => {
            // ТЗ-145, ADR-25: ЦАП не помечен как понимающий DoP — предложить
            // включить настройку плеера и перейти в Оптимальный (DSD→PCM).
            out.player(ParamId::DeviceDop, mode, RecAction::TurnOn);
            out.switch(ModeKind::Optimal, SwitchWhy::DsdToPcm);
        }
        Incompatibility::DriverRejectedRate { .. }
        | Incompatibility::DriverRejectedFormat { .. }
        | Incompatibility::FloatUnsupported => {}
    }
}

/// Правила по динамике: underrun, xrun, RT-приоритет, ограничение DSD→PCM
/// (§6.25, ТЗ-68, ТЗ-129, ТЗ-131).
fn dynamic_rules(mode: ModeKind, d: &DynamicPathState, settings: &ModeSettings, out: &mut Recs) {
    if d.underruns > 0 {
        out.player(ParamId::Buffer, mode, RecAction::Increase);
    }
    if d.xruns > 0 {
        out.player(ParamId::DeviceBuffer, mode, RecAction::Increase);
        // Следующее значение буфера устройства запрещено И-Р22 → и буфер плеера.
        let (buffer, device) = match mode {
            ModeKind::Optimal => (Some(settings.optimal.buffer), settings.optimal.device_buffer),
            ModeKind::Strict => (Some(settings.strict.buffer), settings.strict.device_buffer),
            ModeKind::Compatible => (None, DeviceBuffer::default()),
        };
        if let (Some(buffer), Some(next)) = (buffer, next_device_buffer(device)) {
            if buffers_allowed(buffer, next).is_err() {
                out.player(ParamId::Buffer, mode, RecAction::Increase);
            }
        }
        if d.rt_priority == RtPriorityState::Denied {
            out.push(Recommendation::Os { topic: OsTopic::RealtimePriority });
        }
    }
    if d.dsd_clipped > 0 {
        out.player(ParamId::DsdGainComp, mode, RecAction::TurnOff);
    }
}

/// Следующее большее значение буфера устройства (ОВС-13); `None` у максимума.
fn next_device_buffer(b: DeviceBuffer) -> Option<DeviceBuffer> {
    match b {
        DeviceBuffer::Ms20 => Some(DeviceBuffer::Ms40),
        DeviceBuffer::Ms40 => Some(DeviceBuffer::Ms100),
        DeviceBuffer::Ms100 => Some(DeviceBuffer::Ms200),
        DeviceBuffer::Ms200 => Some(DeviceBuffer::Ms400),
        DeviceBuffer::Ms400 => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use smallvec::SmallVec;

    use super::*;
    use crate::audio::backend::{AccessKind, DeviceCaps, DriverFormat, HwDeviceId, OutputApi, RateSet};
    use crate::audio::format::{ChannelLayout, Codec, Container, OutputSampleFormat, SourceFormat, SourceKind};
    use crate::audio::path::plan::{plan_with_device, with_gain};
    use crate::audio::path::transform::Confidence;
    use crate::audio::path::{plan, OpenedFacts, PathPlan, ReservationView};
    use crate::audio::verdict::GainState;
    use crate::settings::playback::{BufferMs, DeviceOpts, PathPolicy, Volume};

    fn sr(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    fn src(bits: u8, rate: u32) -> SourceFormat {
        SourceFormat {
            container: Container::Flac,
            codec: Codec::Flac,
            lossy: false,
            rate: sr(rate),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::Pcm { bits: BitDepth::new(bits).unwrap() },
        }
    }

    fn caps(access: AccessKind, rates: &[u32]) -> DeviceCaps {
        let api = match access {
            AccessKind::Shared => OutputApi::WasapiShared,
            AccessKind::Exclusive => OutputApi::AlsaHw,
        };
        DeviceCaps {
            api,
            access,
            name: "test".to_owned(),
            rates: RateSet::Discrete(rates.iter().map(|&h| sr(h)).collect()),
            formats: SmallVec::from_slice(&[OutputSampleFormat::S32Le]),
            channels: SmallVec::from_slice(&[2]),
        }
    }

    fn policy(s: &ModeSettings, mode: ModeKind) -> PathPolicy<'_> {
        match mode {
            ModeKind::Compatible => PathPolicy::Compatible(&s.compatible),
            ModeKind::Optimal => PathPolicy::Optimal(&s.optimal),
            ModeKind::Strict => PathPolicy::Strict(&s.strict),
        }
    }

    fn access_of(mode: ModeKind) -> AccessKind {
        match mode {
            ModeKind::Compatible => AccessKind::Shared,
            ModeKind::Optimal | ModeKind::Strict => AccessKind::Exclusive,
        }
    }

    fn opened(p: PathPlan, os: OsStage) -> SignalPath {
        let facts = OpenedFacts {
            access: access_of(p.mode),
            api: OutputApi::AlsaHw,
            device_name: "test".to_owned(),
            driver: DriverFormat::Confirmed(p.target),
            reservation: ReservationView::NotApplicable,
            os,
            device_buffer: None,
        };
        SignalPath::from_opened(p, facts, 1)
    }

    /// 16/44.1 на устройстве с 44.1 в режиме `mode` с усилением `gain`.
    fn path(s: &ModeSettings, mode: ModeKind, gain: GainState, os: OsStage) -> SignalPath {
        let p = plan(&src(16, 44_100), &caps(access_of(mode), &[44_100]), policy(s, mode)).unwrap();
        opened(with_gain(&p, gain, policy(s, mode)), os)
    }

    fn players(recs: &[Recommendation]) -> Vec<(ParamId, RecAction)> {
        recs.iter()
            .filter_map(|r| match r {
                Recommendation::PlayerSetting(p) => Some((p.param(), p.action())),
                _ => None,
            })
            .collect()
    }

    fn has_param(recs: &[Recommendation], param: ParamId) -> bool {
        players(recs).iter().any(|(p, _)| *p == param)
    }

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

    fn hw_device_id() -> HwDeviceId {
        let Ok(id) = "hw:CARD=A,DEV=0".parse::<HwDeviceId>() else {
            panic!("valid HwDeviceId");
        };
        id
    }

    fn pipewire_48() -> OsStage {
        OsStage::Known {
            server: OsServer::PipeWire,
            rate: Some(sr(48_000)),
            format: None,
            confidence: Confidence::Assumed,
        }
    }

    /// §7.2 `recommendations_three_types` (ТЗ-81, ТЗ-68): плеер, ОС, оборудование.
    #[test]
    fn recommendations_three_types() {
        let s = ModeSettings::default();
        let calm = DynamicPathState::default();

        // Громкость 80 % в Оптимальном → «громкость 100 %».
        let vol80 = GainState::Scaled(Volume::new(80).unwrap());
        let p = path(&s, ModeKind::Optimal, vol80, OsStage::NotInvolved);
        let r = recommend(PathSubject::Opened(&p), &calm, &s);
        assert!(players(&r).contains(&(ParamId::Volume, RecAction::SetVolumeFull)), "{r:?}");

        // PipeWire на 48 кГц при потоке 44.1 в Совместимом → «Настройки ОС» + Exclusive.
        let p = path(&s, ModeKind::Compatible, GainState::Unity, pipewire_48());
        let r = recommend(PathSubject::Opened(&p), &calm, &s);
        assert!(r.contains(&Recommendation::Os { topic: OsTopic::PipeWireAllowedRates }), "{r:?}");
        assert!(r.iter().any(|x| matches!(x, Recommendation::SwitchMode { why: SwitchWhy::Exclusive, .. })));

        // 24/192 на устройстве до 96 в Строгом → «Оборудование» + Оптимальный.
        let dev96 = caps(AccessKind::Exclusive, &[44_100, 48_000, 96_000]);
        let blocked = plan(&src(24, 192_000), &dev96, policy(&s, ModeKind::Strict)).unwrap_err();
        let r = recommend(PathSubject::Skipped(&blocked), &calm, &s);
        assert!(r.contains(&Recommendation::Hardware { need: HwRequirement::Rate(sr(192_000)) }), "{r:?}");
        assert!(r.contains(&Recommendation::SwitchMode { to: ModeKind::Optimal, why: SwitchWhy::Resample }));

        // underrun → «увеличить буфер плеера».
        let p = path(&s, ModeKind::Optimal, GainState::Unity, OsStage::NotInvolved);
        let d = DynamicPathState { underruns: 2, ..Default::default() };
        let r = recommend(PathSubject::Opened(&p), &d, &s);
        assert!(players(&r).contains(&(ParamId::Buffer, RecAction::Increase)), "{r:?}");

        // Нет потока — нет рекомендаций.
        assert!(recommend(PathSubject::NoStream, &d, &s).is_empty());
    }

    /// §7.2 `recommendations_underrun_vs_xrun` (ТЗ-68, ТЗ-81, ТЗ-129).
    #[test]
    fn recommendations_underrun_vs_xrun() {
        let mut s = ModeSettings::default();
        s.optimal.buffer = BufferMs::new(1_000).unwrap();
        let p = path(&s, ModeKind::Optimal, GainState::Unity, OsStage::NotInvolved);
        let rt = Recommendation::Os { topic: OsTopic::RealtimePriority };

        let d = DynamicPathState { xruns: 1, ..Default::default() };
        let r = recommend(PathSubject::Opened(&p), &d, &s);
        assert!(has_param(&r, ParamId::DeviceBuffer) && !has_param(&r, ParamId::Buffer), "{r:?}");
        assert!(!r.contains(&rt));

        let d = DynamicPathState { xruns: 1, rt_priority: RtPriorityState::Denied, ..Default::default() };
        let r = recommend(PathSubject::Opened(&p), &d, &s);
        assert!(has_param(&r, ParamId::DeviceBuffer) && r.contains(&rt), "{r:?}");

        let d = DynamicPathState { underruns: 1, ..Default::default() };
        let r = recommend(PathSubject::Opened(&p), &d, &s);
        assert!(has_param(&r, ParamId::Buffer) && !has_param(&r, ParamId::DeviceBuffer), "{r:?}");

        let d = DynamicPathState { dsd_clipped: 7, ..Default::default() };
        let r = recommend(PathSubject::Opened(&p), &d, &s);
        assert!(players(&r).contains(&(ParamId::DsdGainComp, RecAction::TurnOff)), "{r:?}");

        // И-Р22: следующий буфер устройства не помещается в буфер плеера → и `Buffer`.
        s.optimal.buffer = BufferMs::new(100).unwrap();
        s.optimal.device_buffer = DeviceBuffer::Ms40;
        let d = DynamicPathState { xruns: 1, ..Default::default() };
        let r = recommend(PathSubject::Opened(&p), &d, &s);
        assert!(has_param(&r, ParamId::DeviceBuffer) && has_param(&r, ParamId::Buffer), "{r:?}");
    }

    /// §7.2 `recommendations_respect_mode_matrix` (ТЗ-82, И-Т10): целевой
    /// параметр каждой рекомендации `Configurable`; Совместимый при 100 % и
    /// ресемплинге системой → `SwitchMode`.
    #[test]
    fn recommendations_respect_mode_matrix() {
        let s = ModeSettings::default();
        let busy = DynamicPathState {
            underruns: 1,
            xruns: 1,
            rt_priority: RtPriorityState::Denied,
            dsd_clipped: 1,
            lossy_clipped: 1,
            ..Default::default()
        };
        let modes = [ModeKind::Compatible, ModeKind::Optimal, ModeKind::Strict];
        let gains = [GainState::Unity, GainState::Scaled(Volume::new(50).unwrap()), GainState::Muted];
        for mode in modes {
            for gain in gains {
                for os in [OsStage::NotInvolved, OsStage::Unknown, pipewire_48()] {
                    let p = path(&s, mode, gain, os);
                    for (param, _) in players(&recommend(PathSubject::Opened(&p), &busy, &s)) {
                        assert_eq!(availability(param, mode), Availability::Configurable, "{param:?} в {mode:?}");
                    }
                }
            }
        }
        let params = [ParamId::Volume, ParamId::Buffer, ParamId::DeviceBuffer, ParamId::DsdGainComp, ParamId::FixedRate];
        for param in params {
            for mode in modes {
                let ok = availability(param, mode) == Availability::Configurable;
                assert_eq!(PlayerSettingRec::new(param, mode, RecAction::Increase).is_some(), ok);
            }
        }

        // Совместимый, громкость 100 %, система ресемплирует → SwitchMode.
        let p = path(&s, ModeKind::Compatible, GainState::Unity, pipewire_48());
        let r = recommend(PathSubject::Opened(&p), &DynamicPathState::default(), &s);
        assert!(!has_param(&r, ParamId::Volume));
        let to_exclusive = Recommendation::SwitchMode { to: ModeKind::Optimal, why: SwitchWhy::Exclusive };
        assert!(r.contains(&to_exclusive), "{r:?}");
    }

    /// §6.25, ТЗ-145, ADR-25: `DsdToPcm` в Оптимальном из-за `dev.dop = false`
    /// → «Настройки плеера»: `DeviceDop` (включить), без «Оборудование».
    #[test]
    fn recommendation_dsd_to_pcm_device_dop_off_optimal() {
        let s = ModeSettings::default();
        let id = hw_device_id();
        let c = caps(AccessKind::Exclusive, &[44_100, 88_200, 176_400]);
        let p = plan_with_device(
            &src_dsd(DsdRate::Dsd64),
            &c,
            PathPolicy::Optimal(&s.optimal),
            Some((&id, DeviceOpts { dop: false })),
        )
        .unwrap();
        let path = opened(p, OsStage::NotInvolved);
        let r = recommend(PathSubject::Opened(&path), &DynamicPathState::default(), &s);
        assert!(players(&r).contains(&(ParamId::DeviceDop, RecAction::TurnOn)), "{r:?}");
        assert!(
            !r.iter().any(|x| matches!(x, Recommendation::Hardware { need: HwRequirement::DopRate(_) })),
            "{r:?}"
        );
    }

    /// §6.25, ТЗ-145, ADR-25: `Blocked(DopDisabled)` в Строгом → «Настройки
    /// плеера»: `DeviceDop` (включить) + переход в Оптимальный (DSD→PCM).
    #[test]
    fn recommendation_blocked_dop_disabled() {
        let s = ModeSettings::default();
        let id = hw_device_id();
        let c = caps(AccessKind::Exclusive, &[44_100, 88_200, 176_400]);
        let blocked = plan_with_device(
            &src_dsd(DsdRate::Dsd64),
            &c,
            PathPolicy::Strict(&s.strict),
            Some((&id, DeviceOpts { dop: false })),
        )
        .unwrap_err();
        let r = recommend(PathSubject::Skipped(&blocked), &DynamicPathState::default(), &s);
        assert!(players(&r).contains(&(ParamId::DeviceDop, RecAction::TurnOn)), "{r:?}");
        assert!(
            r.contains(&Recommendation::SwitchMode { to: ModeKind::Optimal, why: SwitchWhy::DsdToPcm }),
            "{r:?}"
        );
    }
}

