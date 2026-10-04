use music_player_rs::audio::player::StreamDesc;
use music_player_rs::persist::state_file::{Origin, StateChange};
use music_player_rs::playlist::Track;
use music_player_rs::settings::{DsdMode, ResamplerDither};

/// Данные для отчёта bit-perfect. Отчёт строится по ним, а не по `MusicApp`,
/// чтобы тест не создавал приложение с файлами пользователя (ТЗ-49, ТЗ-51).
#[derive(Debug, Clone, Copy)]
pub struct BpInputs<'a> {
    pub bit_perfect: bool,
    pub stream: Option<&'a StreamDesc>,
    pub track: Option<&'a Track>,
    pub track_is_dsd: bool,
    pub volume: f32,
    pub muted: bool,
    pub dither: ResamplerDither,
}

/// Снимок данных отчёта из состояния приложения.
pub fn bp_inputs(app: &super::MusicApp) -> BpInputs<'_> {
    BpInputs {
        bit_perfect: app.player.bit_perfect(),
        stream: app.player.stream_desc(),
        track: app.current.and_then(|i| app.tracks.get(i)),
        track_is_dsd: app.current_track_is_dsd(),
        volume: app.player.volume(),
        muted: app.player.muted(),
        dither: app.core.settings().playback.audio.resampler.dither,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BpReason {
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    pub action_id: i32,
    pub action_label: String,
}

/// Текст бейджа старого пути: фактические параметры драйвера не читаются,
/// поэтому условия «Bit-perfect» проверить нельзя (ТЗ-52, §8 С1).
pub const BADGE_CHECK_UNAVAILABLE: &str = "Не bit-perfect: проверка недоступна";

/// Состояние бейджа статус-бара старого пути: `(bp_active, текст)`.
/// `bp_active` всегда `false`, пока нет нового тракта (ТЗ-4, ТЗ-113);
/// текст показывается, только если пользователь включил bit-perfect (ТЗ-52, §8 С1).
pub fn status_badge(bit_perfect: bool) -> (bool, &'static str) {
    if bit_perfect {
        (false, BADGE_CHECK_UNAVAILABLE)
    } else {
        (false, "")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BpReport {
    pub bp_active: bool,
    pub badge_text: String,
    pub stream_desc: String,
    pub source_desc: String,
    pub reasons: Vec<BpReason>,
    pub positives: Vec<String>,
}

fn describe_stream_desc(desc: Option<&StreamDesc>) -> String {
    let Some(desc) = desc else {
        return "Нет активного потока".to_string();
    };

    let access = if desc.exclusive { "Exclusive" } else { "Shared" };
    let mut s = format!("{} Гц · {} · {}", desc.rate, desc.format, access);
    if desc.resampled {
        s.push_str(&format!(" · ресемплинг из {}", desc.source_rate));
    }
    s
}

fn current_source_desc(track: Option<&Track>) -> String {
    let Some(track) = track else {
        return "Нет трека".to_string();
    };

    let mut parts = Vec::new();
    if !track.format.is_empty() {
        parts.push(track.format.clone());
    }
    if !track.bit_depth.is_empty() {
        parts.push(track.bit_depth.clone());
    }
    if track.sample_rate > 0 {
        parts.push(format!("{} Hz", track.sample_rate));
    }
    if track.channels > 0 {
        parts.push(format!("{} ch", track.channels));
    }
    if parts.is_empty() {
        "Нет трека".to_string()
    } else {
        parts.join(" · ")
    }
}

pub fn build_bp_report(inp: &BpInputs<'_>) -> BpReport {
    let (bp_active, badge_text) = status_badge(inp.bit_perfect);
    let mut report = BpReport {
        bp_active,
        badge_text: badge_text.to_string(),
        stream_desc: describe_stream_desc(inp.stream),
        source_desc: current_source_desc(inp.track),
        reasons: Vec::new(),
        positives: Vec::new(),
    };

    // ТЗ-52, §8 С1: старый путь не читает фактические параметры драйвера —
    // «Bit-perfect» не подтверждается ни при каких настройках.
    report.reasons.push(BpReason {
        severity: Severity::Info,
        title: "Проверка недоступна".into(),
        detail: "Фактические параметры драйвера не читаются; бейдж «Bit-perfect» не показывается до нового тракта".into(),
        action_id: 0,
        action_label: String::new(),
    });

    let volume = inp.volume;
    let muted = inp.muted;
    let dither = inp.dither;
    let stream = inp.stream;

    if volume < 1.0 {
        report.reasons.push(BpReason {
            severity: Severity::Warning,
            title: format!("Программная громкость активна ({volume:.2})"),
            detail: format!("{:.1} dB ослабление в аудио-колбэке", 20.0 * f32::log10(volume.max(0.0001))),
            action_id: 1,
            action_label: "Поставить 100%".into(),
        });
    }
    if muted {
        report.reasons.push(BpReason {
            severity: Severity::Warning,
            title: "Приглушено".into(),
            detail: "Приглушено в аудио-колбэке".into(),
            action_id: 2,
            action_label: "Включить звук".into(),
        });
    }
    if let Some(desc) = stream {
        if desc.resampled {
            report.reasons.push(BpReason {
                severity: Severity::Warning,
                title: "Ресемплинг включён".into(),
                detail: format!("{} → {} (устройство не поддерживает {})", desc.source_rate, desc.rate, desc.source_rate),
                action_id: 0,
                action_label: String::new(),
            });
        }
        if !desc.exclusive && !desc.exclusive_fallback {
            report.reasons.push(BpReason {
                severity: Severity::Warning,
                title: "Общий доступ (shared)".into(),
                detail: "Устройство работает в shared-режиме".into(),
                action_id: 0,
                action_label: String::new(),
            });
        }
        if desc.exclusive_fallback {
            report.reasons.push(BpReason {
                severity: Severity::Warning,
                title: "Exclusive недоступен".into(),
                detail: "Устройство отклонило exclusive; используется shared".into(),
                action_id: 0,
                action_label: String::new(),
            });
        }
        if desc.dsd_mode == Some(DsdMode::Pcm) && inp.track_is_dsd {
            report.reasons.push(BpReason {
                severity: Severity::Warning,
                title: "Конвертация DSD → PCM".into(),
                detail: "Децимация CIC ломает bit-perfect".into(),
                action_id: 0,
                action_label: String::new(),
            });
        }
        if let Some(reason) = &desc.dsd_fallback_reason {
            report.reasons.push(BpReason {
                severity: Severity::Info,
                title: "DoP применяется вместо Native".into(),
                detail: format!("{reason} (bit-perfect поток сохраняется через DoP)"),
                action_id: 0,
                action_label: String::new(),
            });
        }
    }
    if matches!(dither, ResamplerDither::Tpdf | ResamplerDither::Triangular) {
        report.reasons.push(BpReason {
            severity: Severity::Warning,
            title: "Дизеринг активен".into(),
            detail: format!("Применён {dither:?} дизеринг"),
            action_id: 3,
            action_label: "Отключить дизеринг".into(),
        });
    }

    if let Some(desc) = stream {
        if desc.exclusive {
            report.positives.push("✓ Выдан exclusive-доступ".into());
        }
        if !desc.resampled {
            report.positives.push("✓ Native-частота принята (без ресемплинга)".into());
        }
        if desc.format == "I32" {
            report.positives.push("✓ Принят поток I32".into());
        }
    }
    if matches!(dither, ResamplerDither::Off) {
        report.positives.push("✓ Дизеринг не применяется".into());
    }
    if volume >= 1.0 && !muted {
        report.positives.push("✓ Программная громкость не применяется".into());
    }
    if stream.is_some() {
        report.positives.push("✓ Обнаружено устройство с поддержкой exclusive".into());
    }

    report
}

/// Быстрые исправления из отчёта: громкость/mute — состояние сессии через
/// `change_state`, дизеринг — настройка через `set_settings` (И-Т7, §8.1 С3).
/// Старые поля обновляются до шага очистки, иначе мост откатит значения.
pub fn apply_action(app: &mut super::MusicApp, action_id: i32) {
    match action_id {
        1 => {
            app.player.set_volume(1.0);
            app.settings.settings.volume = 1.0;
            app.core.change_state(Origin::User, StateChange::Volume(100));
        }
        2 => {
            app.player.set_muted(false);
            app.settings.settings.muted = false;
            app.core.change_state(Origin::User, StateChange::Muted(false));
        }
        3 => {
            app.player.set_dither(ResamplerDither::Off);
            app.settings.settings.audio.resampler.dither = ResamplerDither::Off;
            let mut s = app.core.settings().clone();
            s.playback.audio.resampler.dither = ResamplerDither::Off;
            app.core.set_settings(s);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Отчёт строится по данным в памяти: без `MusicApp::new`, окна,
    /// трея и файлов пользователя (ТЗ-49, ТЗ-51).
    #[test]
    fn bp_report_marks_volume_issue() {
        let report = build_bp_report(&BpInputs {
            bit_perfect: true,
            stream: None,
            track: None,
            track_is_dsd: false,
            volume: 0.5,
            muted: false,
            dither: ResamplerDither::Off,
        });
        assert!(report.reasons.iter().any(|r| r.action_id == 1));
        assert!(!report.bp_active);
    }

    /// Даже при идеальных условиях старый путь не показывает «Bit-perfect»:
    /// параметры драйвера не подтверждены (ТЗ-52, §8 С1).
    #[test]
    fn bp_report_never_active_in_old_path() {
        let stream = StreamDesc {
            exclusive: true,
            ..StreamDesc::default()
        };
        let report = build_bp_report(&BpInputs {
            bit_perfect: true,
            stream: Some(&stream),
            track: None,
            track_is_dsd: false,
            volume: 1.0,
            muted: false,
            dither: ResamplerDither::Off,
        });
        assert!(!report.bp_active);
        assert_eq!(report.badge_text, BADGE_CHECK_UNAVAILABLE);
        assert!(report.reasons.iter().any(|r| r.title == "Проверка недоступна"));
    }

    /// Бейдж статус-бара: `bp_active` всегда `false`; текст — только при
    /// включённом bit-perfect (ТЗ-52, §8 С1).
    #[test]
    fn bp_report_status_badge_never_green() {
        assert_eq!(status_badge(true), (false, BADGE_CHECK_UNAVAILABLE));
        assert_eq!(status_badge(false), (false, ""));
    }
}
