//! Вид блока «Информация об устройстве» вкладки «Аудио» (§8 С11,
//! AM1.0-B30). Решение пользователя 2026-10-23 (ROADMAP.md AM1.0-B30):
//! блок под списком устройств — не всегда-пустой `codec-info`, а строка
//! «Сейчас: …» и возможности фактически открытого устройства, построенные
//! из уже собранных `DacCaps`/`OsStage` последнего пути сигнала
//! (`src/audio/path/mod.rs`) без повторного опроса устройства. Для
//! Оптимального/Строгого режима показываются частоты (семействами 44,1 и
//! 48 кГц), разрядность и DSD/DoP; для Совместимого — формат микшера
//! сервера ОС (ОВС-4: матрица фиксирует `F32` для серверов Совместимого
//! режима).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use music_player_rs::audio::backend::RateSet;
use music_player_rs::audio::format::DsdRate;
use music_player_rs::audio::path::{DacCaps, OsStage};
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
    /// «Разрядность: до N бит[, float]» — только Оптимальный/Строгий.
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
/// (`mixer_line`, из `os`); Оптимальный/Строгий — монопольный доступ и
/// возможности ЦАП (`rates_line`/`bits_line`/`dsd_line`, из `dac`).
/// `dac`/`os` читаются как уже собранные данные последнего пути сигнала —
/// `DacCaps::default()`/`OsStage::Unknown`, если ничего не известно, дают
/// текст «неизвестно», а не пустые строки.
pub fn device_info_view(
    mode: ModeKind,
    opened: Option<OpenedDevice<'_>>,
    dac: &DacCaps,
    os: &OsStage,
) -> DeviceInfoView {
    let Some(dev) = opened else {
        return DeviceInfoView {
            now_line: "○ не открыто".to_owned(),
            rates_line: None,
            bits_line: None,
            dsd_line: None,
            mixer_line: None,
        };
    };

    if mode == ModeKind::Compatible {
        DeviceInfoView {
            now_line: format!("Сейчас: ● {} — куда сейчас маршрутизирует сервер", dev.label),
            rates_line: None,
            bits_line: None,
            dsd_line: None,
            mixer_line: Some(mixer_line(os)),
        }
    } else {
        DeviceInfoView {
            now_line: format!("Сейчас: ● {} — монопольно/bit-perfect", dev.label),
            rates_line: Some(rates_line(dac)),
            bits_line: Some(bits_line(dac)),
            dsd_line: Some(dsd_line(dac)),
            mixer_line: None,
        }
    }
}

/// «Частоты: 44,1 кГц: 44,1 / 88,2 / 176,4; 48 кГц: 48 / 96 / 192» —
/// семействами (кратно 44 100 / 48 000 Гц, AM1.0-B30); частоты вне обеих
/// семей — группа «другие»; `Range` — как сплошной диапазон, без семей
/// (нет дискретного списка, который можно разложить).
fn rates_line(dac: &DacCaps) -> String {
    if dac.is_unknown() {
        return "Частоты: неизвестно".to_owned();
    }
    match &dac.rates {
        RateSet::Discrete(rates) => {
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
                groups.push(format!("44,1 кГц: {}", join_khz(&f441)));
            }
            if !f48.is_empty() {
                groups.push(format!("48 кГц: {}", join_khz(&f48)));
            }
            if !other.is_empty() {
                groups.push(format!("другие: {}", join_khz(&other)));
            }
            if groups.is_empty() {
                return "Частоты: неизвестно".to_owned();
            }
            format!("Частоты: {}", groups.join("; "))
        }
        RateSet::Range { min, max } => format!("Частоты: {}–{}", khz(min.hz()), khz(max.hz())),
    }
}

fn join_khz(rates: &[u32]) -> String {
    rates.iter().map(|&hz| khz_num(hz)).collect::<Vec<_>>().join(" / ")
}

/// «Разрядность: до N бит[, float]»/«неизвестно» (как
/// `audio::path::report::bit_depth_have_text`, AM1.0-B29 — та функция
/// `fn`-приватна модулю `audio::path`, здесь повторена для текста блока).
fn bits_line(dac: &DacCaps) -> String {
    if dac.is_unknown() {
        return "Разрядность: неизвестно".to_owned();
    }
    if dac.float_only {
        return "Разрядность: до 24 бит, float".to_owned();
    }
    if dac.max_bits == 0 {
        return "Разрядность: неизвестно".to_owned();
    }
    format!("Разрядность: до {} бит", dac.max_bits)
}

/// «DSD: до DSDxxx (DoP)»/«DSD: DoP не поддерживается»/«неизвестно».
fn dsd_line(dac: &DacCaps) -> String {
    if dac.is_unknown() {
        return "DSD: неизвестно".to_owned();
    }
    match dac.max_dop {
        Some(d) => format!("DSD: до {} (DoP)", dsd_name(d)),
        None => "DSD: DoP не поддерживается".to_owned(),
    }
}

/// «<частота> · float — сервер ресемплирует и микширует» (ОВС-4) либо
/// «неизвестно», если `OsStage` не `Known` или частота не узнана.
fn mixer_line(os: &OsStage) -> String {
    match os {
        OsStage::Known { rate: Some(rate), .. } => {
            format!("{} · float — сервер ресемплирует и микширует", khz(rate.hz()))
        }
        _ => "неизвестно".to_owned(),
    }
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

/// Частота в кГц с десятичной запятой без лишних нулей (как
/// `audio::path::transform::khz`, продублировано по той же причине):
/// 44 100 → «44,1 кГц», 96 000 → «96 кГц».
fn khz(hz: u32) -> String {
    let int = hz / 1000;
    let frac = hz % 1000;
    if frac == 0 {
        return format!("{int} кГц");
    }
    let digits = format!("{frac:03}");
    format!("{int},{} кГц", digits.trim_end_matches('0'))
}

fn khz_num(hz: u32) -> String {
    khz(hz).trim_end_matches(" кГц").to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;
    use music_player_rs::audio::format::SampleRate;
    use smallvec::SmallVec;

    fn r(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    fn discrete(hz: &[u32]) -> RateSet {
        RateSet::Discrete(hz.iter().copied().map(r).collect::<SmallVec<[SampleRate; 16]>>())
    }

    #[test]
    fn closed_device_shows_not_opened_and_nothing_else() {
        let view = device_info_view(ModeKind::Optimal, None, &DacCaps::default(), &OsStage::Unknown);
        assert_eq!(view.now_line, "○ не открыто");
        assert_eq!(view.rates_line, None);
        assert_eq!(view.bits_line, None);
        assert_eq!(view.dsd_line, None);
        assert_eq!(view.mixer_line, None);
    }

    /// Пример из ROADMAP.md AM1.0-B30: MOTU M2 на Оптимальном режиме, обе
    /// семьи частот, 32 бита, DoP до DSD256.
    #[test]
    fn exclusive_device_motu_m2_full_caps() {
        let dac = DacCaps {
            rates: discrete(&[44_100, 88_200, 176_400, 48_000, 96_000, 192_000]),
            max_bits: 32,
            float_only: false,
            channels: SmallVec::from_slice(&[2]),
            max_dop: Some(DsdRate::Dsd256),
        };
        let opened = OpenedDevice { label: "Motu M2 (USB Audio)" };
        let view = device_info_view(ModeKind::Optimal, Some(opened), &dac, &OsStage::NotInvolved);

        assert_eq!(view.now_line, "Сейчас: ● Motu M2 (USB Audio) — монопольно/bit-perfect");
        assert_eq!(
            view.rates_line,
            Some("Частоты: 44,1 кГц: 44,1 / 88,2 / 176,4; 48 кГц: 48 / 96 / 192".to_owned())
        );
        assert_eq!(view.bits_line, Some("Разрядность: до 32 бит".to_owned()));
        assert_eq!(view.dsd_line, Some("DSD: до DSD256 (DoP)".to_owned()));
        assert_eq!(view.mixer_line, None);
    }

    #[test]
    fn exclusive_device_strict_mode_same_suffix_as_optimal() {
        let dac = DacCaps { rates: discrete(&[48_000]), max_bits: 24, float_only: false, ..DacCaps::default() };
        let opened = OpenedDevice { label: "Cayin RU3 (USB Audio)" };
        let view = device_info_view(ModeKind::Strict, Some(opened), &dac, &OsStage::NotInvolved);
        assert_eq!(view.now_line, "Сейчас: ● Cayin RU3 (USB Audio) — монопольно/bit-perfect");
        assert_eq!(view.rates_line, Some("Частоты: 48 кГц: 48".to_owned()));
    }

    #[test]
    fn exclusive_device_unknown_caps_render_as_unknown() {
        let opened = OpenedDevice { label: "Asus Xonar (Digital)" };
        let view = device_info_view(ModeKind::Optimal, Some(opened), &DacCaps::default(), &OsStage::NotInvolved);
        assert_eq!(view.rates_line, Some("Частоты: неизвестно".to_owned()));
        assert_eq!(view.bits_line, Some("Разрядность: неизвестно".to_owned()));
        assert_eq!(view.dsd_line, Some("DSD: неизвестно".to_owned()));
    }

    #[test]
    fn exclusive_device_float_only_and_no_dop() {
        let dac = DacCaps {
            rates: discrete(&[44_100]),
            max_bits: 0,
            float_only: true,
            channels: SmallVec::new(),
            max_dop: None,
        };
        let opened = OpenedDevice { label: "Realtek" };
        let view = device_info_view(ModeKind::Optimal, Some(opened), &dac, &OsStage::NotInvolved);
        assert_eq!(view.bits_line, Some("Разрядность: до 24 бит, float".to_owned()));
        assert_eq!(view.dsd_line, Some("DSD: DoP не поддерживается".to_owned()));
    }

    #[test]
    fn exclusive_device_range_rates_shown_as_plain_range() {
        let dac = DacCaps { rates: RateSet::Range { min: r(8_000), max: r(192_000) }, max_bits: 16, ..DacCaps::default() };
        let opened = OpenedDevice { label: "Legacy DAC" };
        let view = device_info_view(ModeKind::Optimal, Some(opened), &dac, &OsStage::NotInvolved);
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
        let view = device_info_view(ModeKind::Compatible, Some(opened), &DacCaps::default(), &os);

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
        let view = device_info_view(ModeKind::Compatible, Some(opened), &DacCaps::default(), &OsStage::Unknown);
        assert_eq!(view.mixer_line, Some("неизвестно".to_owned()));
    }
}
