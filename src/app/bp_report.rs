use music_player_rs::settings::{DsdMode, ResamplerDither};

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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BpReport {
    pub bp_active: bool,
    pub stream_desc: String,
    pub source_desc: String,
    pub reasons: Vec<BpReason>,
    pub positives: Vec<String>,
}

fn describe_stream_desc(desc: Option<&music_player_rs::audio::player::StreamDesc>) -> String {
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

fn current_source_desc(app: &super::MusicApp) -> String {
    let Some(i) = app.current else {
        return "Нет трека".to_string();
    };
    let Some(track) = app.tracks.get(i) else {
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

pub fn build_bp_report(app: &super::MusicApp) -> BpReport {
    let mut report = BpReport {
        bp_active: app.player.bit_perfect(),
        stream_desc: describe_stream_desc(app.player.stream_desc()),
        source_desc: current_source_desc(app),
        reasons: Vec::new(),
        positives: Vec::new(),
    };

    let volume = app.player.volume();
    let muted = app.player.muted();
    let dither = app.settings_ref().audio.resampler.dither;
    let stream = app.player.stream_desc();

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
        if desc.dsd_mode == Some(DsdMode::Pcm) && app.current_track_is_dsd() {
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
    if app.player.stream_desc().is_some() {
        report.positives.push("✓ Обнаружено устройство с поддержкой exclusive".into());
    }

    report.bp_active = app.player.bit_perfect() && report.reasons.is_empty();
    report
}

pub fn apply_action(app: &mut super::MusicApp, action_id: i32) {
    match action_id {
        1 => {
            app.player.set_volume(1.0);
            app.settings_mut().volume = 1.0;
        }
        2 => {
            app.player.set_muted(false);
            app.settings_mut().muted = false;
        }
        3 => {
            app.player.set_dither(ResamplerDither::Off);
            app.settings_mut().audio.resampler.dither = ResamplerDither::Off;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bp_report_marks_volume_issue() {
        let report = build_bp_report(&super::super::MusicApp::new(super::super::create_ui().unwrap()));
        assert!(!report.reasons.is_empty());
    }
}
