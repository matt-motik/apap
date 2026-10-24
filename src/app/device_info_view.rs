//! Вид блока «Информация об устройстве» вкладки «Аудио» (§8 С11,
//! AM1.0-B30). Решение пользователя 2026-10-23 (ROADMAP.md AM1.0-B30):
//! блок под списком устройств — не всегда-пустой `codec-info`, а строка
//! «Сейчас: …» и возможности устройства, выбранного в редактируемом режиме.
//! Для Оптимального/Строгого — частоты (семействами 44,1 и 48 кГц),
//! разрядность с перечнем форматов поля и DSD/DoP; для Совместимого —
//! формат микшера сервера ОС (ОВС-4: матрица фиксирует `F32` для серверов
//! Совместимого режима).
//!
//! Решение пользователя 2026-10-24 (ROADMAP.md AM1.0-B30, кэш возможностей
//! открытых устройств): «Сейчас: …» остаётся строкой фактически открытого
//! устройства (`opened`), но частоты/разрядность/DSD строятся из `DeviceCaps`
//! выбранного устройства РЕДАКТИРУЕМОГО режима — он может отличаться от
//! фактически открытого (другой режим ещё не сохранён/не активен). `caps`
//! читается из кэша по `HwDeviceId` (`src/app/ui_audio_state.rs`), собранного
//! движком при открытии устройства (`EngineEvent::DeviceCapsCached`), без
//! повторного опроса устройства (ТЗ не задан отдельно — решение шага);
//! `None` — устройство этого режима ещё не открывалось в этой сессии.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use music_player_rs::audio::backend::{AccessKind, DeviceCaps, RateSet};
use music_player_rs::audio::format::{DsdRate, OutputSampleFormat, SampleRate};
use music_player_rs::audio::path::OsStage;
use music_player_rs::i18n;
use music_player_rs::settings::playback::ModeKind;

/// Фактически открытое устройство на момент построения вида (AM1.0-B30):
/// подпись каталога (`hw_device_label`/системное) — не выбор в `ComboBox`,
/// который может ещё не быть сохранён (ТЗ-94, §2.4).
#[derive(Clone, Copy, Debug)]
pub struct OpenedDevice<'a> {
    pub label: &'a str,
}

/// Вид блока «Информация об устройстве» (AM1.0-B30). Поля — уже
/// отрендеренный текст для `Text`/`RowLabel` Slint; `None` — строка не
/// показывается (не относится к текущему режиму/состоянию устройства).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DeviceInfoView {
    /// «Сейчас: ● <устройство> — …» либо «○ не открыто».
    pub now_line: String,
    /// «Частоты: 44,1 кГц: …; 48 кГц: …» — только Оптимальный/Строгий.
    pub rates_line: Option<String>,
    /// «Разрядность: N / M бит (КОД, КОД…)» — только Оптимальный/Строгий.
    pub bits_line: Option<String>,
    /// «DSD: до DSDxxx (DoP)»/«DSD: DoP не поддерживается» — только
    /// Оптимальный/Строгий.
    pub dsd_line: Option<String>,
    /// «<частота> · float — сервер ресемплирует и микширует» — только
    /// Совместимый.
    pub mixer_line: Option<String>,
}

/// Строит `DeviceInfoView` (AM1.0-B30). `opened` — `None`, если устройство
/// фактически не открыто (трек не играет/на паузе ни на одном устройстве) —
/// тогда показывается только «○ не открыто», остальные строки скрыты
/// (`None`). При открытом устройстве `mode` решает, что показывать:
/// Совместимый — куда маршрутизирует сервер и формат его микшера
/// (`mixer_line`, из `os`); Оптимальный/Строгий — возможности устройства,
/// выбранного в редактируемом режиме (`rates_line`/`bits_line`/`dsd_line`,
/// из `caps`, кэш по `HwDeviceId`, без повторного опроса устройства) — это
/// не обязательно то же устройство, что в `opened` (§2.4, ТЗ-94: выбор в
/// диалоге и фактически играющее устройство различаются до «Сохранить»).
/// `caps` — `None`, если устройство этого режима ещё не открывалось в этой
/// сессии: даёт `i18n::audio::DEVICE_NOT_CACHED`, а не пустые строки.
pub fn device_info_view(
    mode: ModeKind,
    opened: Option<OpenedDevice<'_>>,
    caps: Option<&DeviceCaps>,
    os: &OsStage,
) -> DeviceInfoView {
    let Some(dev) = opened else {
        return DeviceInfoView {
            now_line: i18n::audio::DEVICE_NOW_CLOSED.to_owned(),
            rates_line: None,
            bits_line: None,
            dsd_line: None,
            mixer_line: None,
        };
    };

    if mode == ModeKind::Compatible {
        DeviceInfoView {
            now_line: i18n::audio::device_now_compatible(dev.label),
            rates_line: None,
            bits_line: None,
            dsd_line: None,
            mixer_line: Some(mixer_line(os)),
        }
    } else {
        DeviceInfoView {
            now_line: i18n::audio::device_now_exclusive(dev.label),
            rates_line: Some(rates_line(caps)),
            bits_line: Some(bits_line(caps)),
            dsd_line: Some(dsd_line(caps)),
            mixer_line: None,
        }
    }
}

/// «Частоты: 44,1 кГц: 44,1 / 88,2 / 176,4; 48 кГц: 48 / 96 / 192» —
/// семействами (кратно 44 100 / 48 000 Гц, AM1.0-B30); частоты вне обеих
/// семей — группа «другие»; `Range` — как сплошной диапазон, без семей
/// (нет дискретного списка, который можно разложить). `None` — устройство
/// не кэшировано (`i18n::audio::DEVICE_NOT_CACHED`).
fn rates_line(caps: Option<&DeviceCaps>) -> String {
    let Some(caps) = caps else {
        return i18n::audio::device_rates_not_cached();
    };
    match &caps.rates {
        RateSet::Discrete(rates) => {
            if rates.is_empty() {
                return i18n::audio::DEVICE_RATES_UNKNOWN.to_owned();
            }
            let mut f441: Vec<u32> = Vec::new();
            let mut f48: Vec<u32> = Vec::new();
            let mut other: Vec<u32> = Vec::new();
            for r in rates {
                let hz = r.hz();
                if hz % 44_100 == 0 {
                    f441.push(hz);
                } else if hz % 48_000 == 0 {
                    f48.push(hz);
                } else {
                    other.push(hz);
                }
            }
            let mut groups: Vec<String> = Vec::new();
            if !f441.is_empty() {
                groups.push(i18n::audio::device_rate_group_441(&join_khz(&f441)));
            }
            if !f48.is_empty() {
                groups.push(i18n::audio::device_rate_group_48(&join_khz(&f48)));
            }
            if !other.is_empty() {
                groups.push(i18n::audio::device_rate_group_other(&join_khz(&other)));
            }
            if groups.is_empty() {
                return i18n::audio::DEVICE_RATES_UNKNOWN.to_owned();
            }
            i18n::audio::device_rates_line(&groups.join("; "))
        }
        RateSet::Range { min, max } => i18n::audio::device_rates_range(&khz(min.hz()), &khz(max.hz())),
    }
}

fn join_khz(rates: &[u32]) -> String {
    rates.iter().map(|&hz| khz_num(hz)).collect::<Vec<_>>().join(" / ")
}

/// «<частота> · float — сервер ресемплирует и микширует» (ОВС-4) либо
/// «неизвестно», если `OsStage` не `Known` или частота не узнана.
fn mixer_line(os: &OsStage) -> String {
    match os {
        OsStage::Known { rate: Some(rate), .. } => i18n::audio::device_mixer_line(&khz(rate.hz())),
        _ => i18n::audio::DEVICE_MIXER_UNKNOWN.to_owned(),
    }
}

/// «Разрядность: 16 / 24 / 32 бит (S16_LE, S24_3LE, S32_LE)» — решение
/// пользователя 2026-10-24 (ROADMAP.md AM1.0-B30): в отличие от прежнего
/// текста («до N бит»), формат поля показывается списком — несколько
/// целых форматов с разным числом байт (например, `S24_3LE` и `S24_LE`)
/// дают одно и то же число бит дважды в списке кодов, но не дублируют
/// число в списке битов (`bits`, без повтора). Только `F32` среди
/// `caps.formats` — «до 24 бит, float» (как раньше, без списка кодов: один
/// формат не нуждается в перечне). `None` — устройство не кэшировано
/// (`i18n::audio::DEVICE_NOT_CACHED`); пустой `caps.formats` — «неизвестно»
/// (кэш есть, но без данных — защитный случай, вживую не встречается у ALSA).
fn bits_line(caps: Option<&DeviceCaps>) -> String {
    let Some(caps) = caps else {
        return i18n::audio::device_bits_not_cached();
    };
    if caps.formats.is_empty() {
        return i18n::audio::DEVICE_BITS_UNKNOWN.to_owned();
    }
    let has_int = caps.formats.iter().any(|&f| f != OutputSampleFormat::F32);
    if !has_int {
        return i18n::audio::DEVICE_BITS_FLOAT_ONLY.to_owned();
    }
    let mut bits: Vec<u8> = Vec::new();
    let mut codes: Vec<&'static str> = Vec::new();
    for &fmt in caps.formats.iter() {
        if fmt == OutputSampleFormat::F32 {
            continue;
        }
        let field_bits = fmt.field_bits();
        if !bits.contains(&field_bits) {
            bits.push(field_bits);
        }
        codes.push(format_code(fmt));
    }
    let bits_text = bits.iter().map(u8::to_string).collect::<Vec<_>>().join(" / ");
    i18n::audio::device_bits_line(&bits_text, &codes.join(", "))
}

/// Код формата поля ALSA (ADR-24, ТЗ-109): те же обозначения, что в
/// доке-комментариях `audio::backend::alsa`/`audio::path::plan`
/// (`S16_LE`/`S24_3LE`/`S24_LE`/`S32_LE`/`F32`) — не русский текст
/// `audio::path::report::sample_name` (другая аудитория: там «24 бит (в
/// 32)» для бейджа пути, здесь имя формата поля для списка «Разрядность»).
fn format_code(fmt: OutputSampleFormat) -> &'static str {
    match fmt {
        OutputSampleFormat::S16Le => "S16_LE",
        OutputSampleFormat::S24_3Le => "S24_3LE",
        OutputSampleFormat::S24Le => "S24_LE",
        OutputSampleFormat::S32Le => "S32_LE",
        OutputSampleFormat::F32 => "F32",
    }
}

/// «DSD: до DSDxxx (DoP)»/«DSD: DoP не поддерживается»/
/// `i18n::audio::DEVICE_NOT_CACHED`.
fn dsd_line(caps: Option<&DeviceCaps>) -> String {
    let Some(caps) = caps else {
        return i18n::audio::device_dsd_not_cached();
    };
    match max_dop_for_display(caps) {
        Some(d) => i18n::audio::device_dsd_line(dsd_name(d)),
        None => i18n::audio::DEVICE_DSD_NO_DOP.to_owned(),
    }
}

/// Контейнерные целые форматы DoP в порядке предпочтения (ADR-12) — то же
/// подмножество, что `audio::path::plan::dop_format` пробует по порядку;
/// здесь достаточно знать, есть ли хоть один (список без трека не выбирает
/// конкретный формат).
const DOP_CONTAINERS: [OutputSampleFormat; 3] =
    [OutputSampleFormat::S24_3Le, OutputSampleFormat::S24Le, OutputSampleFormat::S32Le];

/// Частота PCM-контейнера DoP: `dsd / 16` (как `audio::path::plan::dop_rate`,
/// §6.9).
fn dop_rate_hz(dsd: DsdRate) -> u32 {
    dsd.bit_rate_hz() / 16
}

/// Возможность DoP устройства для стереопары — представительный случай для
/// показа «возможностей устройства» без открытого трека (AM1.0-B30,
/// решение 2026-10-24): `audio::path::plan::max_dop`/`dop_format`
/// определяют DoP по каналам и `dev.dop` конкретного открытого трека и
/// недоступны отсюда (приватны модулю, завязаны на трек); здесь —
/// упрощённая, но та же по сути проверка на стерео (2 канала, самый частый
/// случай) без зависимости от настройки `dev.dop` конкретного режима:
/// монопольный доступ, частота контейнера `dsd / 16` среди `caps.rates`,
/// стерео среди `caps.channels`, хотя бы один 24-/32-битный контейнер DoP
/// среди `caps.formats`. Наибольшая подходящая `DsdRate` — как
/// `audio::path::plan::max_dop`.
fn max_dop_for_display(caps: &DeviceCaps) -> Option<DsdRate> {
    if caps.access != AccessKind::Exclusive || !caps.channels.contains(&2) {
        return None;
    }
    DsdRate::ALL.into_iter().rev().find(|&dsd| {
        let Some(rate) = SampleRate::new(dop_rate_hz(dsd)) else {
            return false;
        };
        caps.rates.contains(rate) && DOP_CONTAINERS.iter().any(|&f| caps.formats.contains(&f))
    })
}

/// Имя DSD-частоты (как `audio::path::transform::dsd_name`, `pub(super)` и
/// недоступна из `app` — повторена здесь).
fn dsd_name(dsd: DsdRate) -> &'static str {
    match dsd {
        DsdRate::Dsd64 => "DSD64",
        DsdRate::Dsd128 => "DSD128",
        DsdRate::Dsd256 => "DSD256",
        DsdRate::Dsd512 => "DSD512",
    }
}

/// Частота в кГц с десятичной запятой без лишних нулей (T1.0-B2,
/// [`i18n::format_khz`]): 44 100 → «44,1 кГц», 96 000 → «96 кГц».
fn khz(hz: u32) -> String {
    i18n::format_khz(f64::from(hz) / 1000.0)
}

/// Та же частота без суффикса «кГц» — для списков через « / » (T1.0-B2,
/// [`i18n::audio::format_khz_number`]).
fn khz_num(hz: u32) -> String {
    i18n::audio::format_khz_number(f64::from(hz) / 1000.0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;
    use music_player_rs::audio::backend::OutputApi;
    use smallvec::SmallVec;

    fn r(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    fn discrete(hz: &[u32]) -> RateSet {
        RateSet::Discrete(hz.iter().copied().map(r).collect::<SmallVec<[SampleRate; 16]>>())
    }

    fn caps(rates: RateSet, formats: &[OutputSampleFormat], channels: &[u8]) -> DeviceCaps {
        DeviceCaps {
            api: OutputApi::AlsaHw,
            access: AccessKind::Exclusive,
            name: String::new(),
            rates,
            formats: formats.iter().copied().collect(),
            channels: channels.iter().copied().collect(),
        }
    }

    #[test]
    fn closed_device_shows_not_opened_and_nothing_else() {
        let view = device_info_view(ModeKind::Optimal, None, None, &OsStage::Unknown);
        assert_eq!(view.now_line, "○ не открыто");
        assert_eq!(view.rates_line, None);
        assert_eq!(view.bits_line, None);
        assert_eq!(view.dsd_line, None);
        assert_eq!(view.mixer_line, None);
    }

    /// Пример из ROADMAP.md AM1.0-B30: MOTU M2-подобный ЦАП на Оптимальном
    /// режиме, обе семьи частот (плюс 705,6 кГц — контейнер DoP DSD256),
    /// три формата поля, DoP до DSD256 на стерео.
    #[test]
    fn exclusive_device_motu_m2_full_caps() {
        let dev_caps = caps(
            discrete(&[44_100, 88_200, 176_400, 705_600, 48_000, 96_000, 192_000]),
            &[OutputSampleFormat::S16Le, OutputSampleFormat::S24_3Le, OutputSampleFormat::S32Le],
            &[2],
        );
        let opened = OpenedDevice { label: "Motu M2 (USB Audio)" };
        let view = device_info_view(ModeKind::Optimal, Some(opened), Some(&dev_caps), &OsStage::NotInvolved);

        assert_eq!(view.now_line, "Сейчас: ● Motu M2 (USB Audio) — монопольно/bit-perfect");
        assert_eq!(
            view.rates_line,
            Some("Частоты: 44,1 кГц: 44,1 / 88,2 / 176,4 / 705,6; 48 кГц: 48 / 96 / 192".to_owned())
        );
        assert_eq!(view.bits_line, Some("Разрядность: 16 / 24 / 32 бит (S16_LE, S24_3LE, S32_LE)".to_owned()));
        assert_eq!(view.dsd_line, Some("DSD: до DSD256 (DoP)".to_owned()));
        assert_eq!(view.mixer_line, None);
    }

    #[test]
    fn exclusive_device_strict_mode_same_suffix_as_optimal() {
        let dev_caps = caps(discrete(&[48_000]), &[OutputSampleFormat::S24_3Le], &[2]);
        let opened = OpenedDevice { label: "Cayin RU3 (USB Audio)" };
        let view = device_info_view(ModeKind::Strict, Some(opened), Some(&dev_caps), &OsStage::NotInvolved);
        assert_eq!(view.now_line, "Сейчас: ● Cayin RU3 (USB Audio) — монопольно/bit-perfect");
        assert_eq!(view.rates_line, Some("Частоты: 48 кГц: 48".to_owned()));
    }

    /// Выбранное устройство режима ещё не открывалось в этой сессии — кэш
    /// возможностей для него пуст (AM1.0-B30, решение 2026-10-24).
    #[test]
    fn not_cached_device_shows_hint_on_every_line() {
        let opened = OpenedDevice { label: "Asus Xonar (Digital)" };
        let view = device_info_view(ModeKind::Optimal, Some(opened), None, &OsStage::NotInvolved);
        assert_eq!(view.rates_line, Some("Частоты: неизвестно — устройство ещё не открывалось".to_owned()));
        assert_eq!(view.bits_line, Some("Разрядность: неизвестно — устройство ещё не открывалось".to_owned()));
        assert_eq!(view.dsd_line, Some("DSD: неизвестно — устройство ещё не открывалось".to_owned()));
    }

    /// Кэшировано, но данные пусты — защитный случай (вживую у ALSA не
    /// встречается: `device_caps_of` либо отдаёт заполненные поля, либо
    /// ошибку, которая кэш не трогает).
    #[test]
    fn cached_but_empty_caps_render_as_plain_unknown() {
        let dev_caps = caps(RateSet::Discrete(SmallVec::new()), &[], &[]);
        let opened = OpenedDevice { label: "Asus Xonar (Digital)" };
        let view = device_info_view(ModeKind::Optimal, Some(opened), Some(&dev_caps), &OsStage::NotInvolved);
        assert_eq!(view.rates_line, Some("Частоты: неизвестно".to_owned()));
        assert_eq!(view.bits_line, Some("Разрядность: неизвестно".to_owned()));
        assert_eq!(view.dsd_line, Some("DSD: DoP не поддерживается".to_owned()));
    }

    #[test]
    fn exclusive_device_float_only_and_no_dop() {
        let dev_caps = caps(discrete(&[44_100]), &[OutputSampleFormat::F32], &[]);
        let opened = OpenedDevice { label: "Realtek" };
        let view = device_info_view(ModeKind::Optimal, Some(opened), Some(&dev_caps), &OsStage::NotInvolved);
        assert_eq!(view.bits_line, Some("Разрядность: до 24 бит, float".to_owned()));
        assert_eq!(view.dsd_line, Some("DSD: DoP не поддерживается".to_owned()));
    }

    #[test]
    fn exclusive_device_range_rates_shown_as_plain_range() {
        let dev_caps = caps(RateSet::Range { min: r(8_000), max: r(192_000) }, &[OutputSampleFormat::S16Le], &[2]);
        let opened = OpenedDevice { label: "Legacy DAC" };
        let view = device_info_view(ModeKind::Optimal, Some(opened), Some(&dev_caps), &OsStage::NotInvolved);
        assert_eq!(view.rates_line, Some("Частоты: 8 кГц–192 кГц".to_owned()));
    }

    /// Пример из ROADMAP.md AM1.0-B30: Совместимый, PipeWire на 48 кГц.
    #[test]
    fn compatible_shows_mixer_line_from_os_stage() {
        let opened = OpenedDevice { label: "Системное по умолчанию (PipeWire)" };
        let os = OsStage::Known {
            server: music_player_rs::audio::path::OsServer::PipeWire,
            rate: Some(r(48_000)),
            format: None,
            confidence: music_player_rs::audio::path::transform::Confidence::Assumed,
        };
        let view = device_info_view(ModeKind::Compatible, Some(opened), None, &os);

        assert_eq!(
            view.now_line,
            "Сейчас: ● Системное по умолчанию (PipeWire) — куда сейчас маршрутизирует сервер"
        );
        assert_eq!(view.mixer_line, Some("48 кГц · float — сервер ресемплирует и микширует".to_owned()));
        assert_eq!(view.rates_line, None);
        assert_eq!(view.bits_line, None);
        assert_eq!(view.dsd_line, None);
    }

    #[test]
    fn compatible_unknown_os_stage_shows_unknown_mixer_line() {
        let opened = OpenedDevice { label: "Системное по умолчанию (PipeWire)" };
        let view = device_info_view(ModeKind::Compatible, Some(opened), None, &OsStage::Unknown);
        assert_eq!(view.mixer_line, Some("неизвестно".to_owned()));
    }
}
