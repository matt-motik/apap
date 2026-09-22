//! Machine-checkable proof of the remaining timing/CPU rows in
//! spec_visualizer_v5.1.md §11.1, for non-DSD (PCM) tracks:
//!   - LiveWorker: "Задержка отрисовки ≤ 50 мс", "Частота обновления UI 30-60 FPS"
//!   - LiveWorker: "Нагрузка worker (FFT) ≤ 5% одного ядра"
//!   - FullTrackWorker: "Построение осциллограммы (5 мин) ≤ 1 с"
//!   - FullTrackWorker: "Построение спектрограммы (5 мин, nfft=2048) ≤ 2 с"
//!
//! Per the user's scope decision, the DSD64/DSD512 full-track build-time
//! rows are intentionally NOT tested here: full-track oscillogram/spectrogram
//! visualization is meant to be unavailable for DSD tracks (see
//! `skip_fulltrack_for_dsd`), so those budget rows don't apply. DSD512
//! *playback* CPU load is covered separately in
//! tests/dsd512_playback_cpu_budget.rs.
//!
//! The three timing-based tests are gated to release builds (skip + stderr
//! note in debug) — an unoptimized debug build's FFT/DSP math is not
//! representative of the number these budgets assume.

use std::time::Instant;

use music_player_rs::audio::analyzer::SpectrumEngine;
use music_player_rs::audio::fulltrack::Envelope;
use music_player_rs::audio::spectrogram::Spectrogram;
use music_player_rs::audio::visualizer::{SpectrogramCfg, SpectrumCfg};

/// spec_visualizer_v5.1.md §11.1, LiveWorker row: "Задержка отрисовки ≤ 50 мс"
/// / "Частота обновления UI 30-60 FPS". The UI push timer interval *is* the
/// draw-latency ceiling (worst case: data ready right after a tick, next
/// push is one interval away) — a constant check here is a permanent,
/// zero-flakiness regression guard, unlike trying to time an actual Slint
/// event loop.
#[test]
fn viz_push_interval_meets_latency_and_fps_budget() {
    use music_player_rs::audio::visualizer::VIZ_PUSH_INTERVAL_MS;

    const MAX_LATENCY_MS: u64 = 50;
    const MIN_FPS: f64 = 30.0;
    const MAX_FPS: f64 = 60.0;

    // Both operands are compile-time constants: fails the build itself
    // (not just this test) the moment either constant drifts out of budget.
    const { assert!(VIZ_PUSH_INTERVAL_MS <= MAX_LATENCY_MS, "VIZ_PUSH_INTERVAL_MS exceeds the §11.1 draw-latency budget") };
    let fps = 1000.0 / VIZ_PUSH_INTERVAL_MS as f64;
    assert!(
        (MIN_FPS..=MAX_FPS).contains(&fps),
        "VIZ_PUSH_INTERVAL_MS={VIZ_PUSH_INTERVAL_MS}ms implies {fps:.1} FPS, outside the \
         §11.1 budget of {MIN_FPS}-{MAX_FPS} FPS"
    );
}

fn skip_in_debug(test_name: &str) -> bool {
    if cfg!(debug_assertions) {
        eprintln!(
            "SKIP {test_name}: unoptimized debug-build timings aren't representative of the \
             spec_visualizer_v5.1.md §11.1 budget (assumes release). Run with \
             `cargo test --release --test perf_budgets` to enforce it."
        );
        true
    } else {
        false
    }
}

/// spec_visualizer_v5.1.md §11.1, LiveWorker row: "Нагрузка worker (FFT) ≤ 5%
/// одного ядра" — measured as CPU time / represented-audio-time (real-time
/// factor), not an OS CPU% sample: deterministic and platform-independent.
#[test]
fn live_worker_fft_cpu_budget() {
    if skip_in_debug("live_worker_fft_cpu_budget") {
        return;
    }
    const CPU_BUDGET_RATIO: f64 = 0.05;
    const RATE: u32 = 44_100;
    const CH: usize = 2;
    const AUDIO_SECONDS: f64 = 5.0;

    let cfg = SpectrumCfg::default();
    let mut engine = SpectrumEngine::new(&cfg, RATE, CH);
    let total_samples = (RATE as f64 * AUDIO_SECONDS) as usize * CH;

    let (mut producer, mut ring) = rtrb::RingBuffer::<f32>::new(total_samples + 4096);
    for i in 0..total_samples {
        let _ = producer.push(((i % 997) as f32 / 997.0) - 0.5);
    }

    let mut out = Vec::new();
    let start = Instant::now();
    while engine.consume(&mut ring, &cfg, &mut out) {}
    let elapsed = start.elapsed();

    let real_time_factor = elapsed.as_secs_f64() / AUDIO_SECONDS;
    eprintln!(
        "live_worker_fft_cpu_budget: {:.4}s CPU for {AUDIO_SECONDS}s audio -> \
         real_time_factor={real_time_factor:.4} (budget <= {CPU_BUDGET_RATIO})",
        elapsed.as_secs_f64()
    );
    assert!(
        real_time_factor <= CPU_BUDGET_RATIO,
        "SpectrumEngine::consume took {:.4}s CPU for {AUDIO_SECONDS}s of audio \
         (real-time factor {real_time_factor:.4}) — exceeds the §11.1 budget of \
         {CPU_BUDGET_RATIO} (≤5% of one core)",
        elapsed.as_secs_f64()
    );
}

/// spec_visualizer_v5.1.md §11.1, FullTrackWorker row: "Построение
/// осциллограммы (5 мин) ≤ 1 с".
#[test]
fn oscillogram_build_time_budget() {
    if skip_in_debug("oscillogram_build_time_budget") {
        return;
    }
    const BUDGET_SECS: f64 = 1.0;
    const RATE: u64 = 44_100;
    const CH: usize = 2;
    const TRACK_SECS: u64 = 5 * 60;
    const COLUMNS: usize = 4000;

    let total_frames = RATE * TRACK_SECS;
    let mut env = Envelope::new(total_frames, CH, COLUMNS);
    let chunk = vec![0.25f32; 4096 * CH];
    let full_chunks = (total_frames as usize) / 4096;

    let start = Instant::now();
    for _ in 0..full_chunks {
        env.feed(&chunk, CH);
    }
    let _ = env.finish();
    let elapsed = start.elapsed();

    eprintln!("oscillogram_build_time_budget: {:.4}s (budget <= {BUDGET_SECS}s)", elapsed.as_secs_f64());
    assert!(
        elapsed.as_secs_f64() <= BUDGET_SECS,
        "Building a {TRACK_SECS}s oscillogram took {:.4}s — exceeds the §11.1 budget of {BUDGET_SECS}s",
        elapsed.as_secs_f64()
    );
}

/// spec_visualizer_v5.1.md §11.1, FullTrackWorker row: "Построение
/// спектрограммы (5 мин, nfft=2048) ≤ 2 с". `SpectrogramCfg::default()`
/// already has `fft_size == 2048`, matching this row's parenthetical.
#[test]
fn spectrogram_build_time_budget() {
    if skip_in_debug("spectrogram_build_time_budget") {
        return;
    }
    const BUDGET_SECS: f64 = 2.0;
    const RATE: u32 = 44_100;
    const CH: usize = 2;
    const TRACK_SECS: u64 = 5 * 60;

    let cfg = SpectrogramCfg::default();
    assert_eq!(cfg.fft_size, 2048, "this budget row assumes nfft=2048 per spec_visualizer_v5.1.md §11.1");

    let total_frames = RATE as u64 * TRACK_SECS;
    let mut spec = Spectrogram::new(total_frames, RATE, CH, false, &cfg)
        .expect("Spectrogram::new should succeed for a normal track");
    let chunk = vec![0.25f32; 4096 * CH];
    let full_chunks = (total_frames as usize) / 4096;

    let start = Instant::now();
    for _ in 0..full_chunks {
        spec.feed(&chunk, CH);
    }
    let _ = spec.finish();
    let elapsed = start.elapsed();

    eprintln!("spectrogram_build_time_budget: {:.4}s (budget <= {BUDGET_SECS}s)", elapsed.as_secs_f64());
    assert!(
        elapsed.as_secs_f64() <= BUDGET_SECS,
        "Building a {TRACK_SECS}s spectrogram (nfft=2048) took {:.4}s — exceeds the §11.1 budget of {BUDGET_SECS}s",
        elapsed.as_secs_f64()
    );
}
