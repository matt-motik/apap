//! Тест-детектор аллокаций в колбэке вывода (AM1.0 §7.2, ТЗ-75: «0 аллокаций в
//! колбэке»). Каждый рендер (`PcmRender` i32/f32 × I16/I24/I32/F32 ×
//! `NoGain`/`AtomicGain` × TPDF вкл/выкл, `DopRender` I24/I32) прогоняется через
//! все состояния колбэка: воспроизведение, пауза, priming, seek (фаза 3),
//! underrun — контракт должен держаться на каждом пути (§6.14–§6.17).
//!
//! Отдельный бинарник интеграционного теста: `#[global_allocator]` ниже
//! инструментирует только его, а не приложение.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingAllocator;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static DEALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn counts() -> (usize, usize) {
    (ALLOC_COUNT.load(Ordering::SeqCst), DEALLOC_COUNT.load(Ordering::SeqCst))
}

/// Runs `f` `iterations` times and asserts it caused zero alloc/dealloc
/// calls (not "zero net growth" — a matched alloc+dealloc pair is still a
/// hot-path allocation and must fail this check).
fn assert_zero_alloc<F: FnMut()>(name: &str, iterations: usize, mut f: F) {
    let before = counts();
    for _ in 0..iterations {
        f();
    }
    let after = counts();
    assert_eq!(
        before, after,
        "{name}: allocator was touched during {iterations} RT-path calls \
         (alloc {}->{}, dealloc {}->{}) — RT callbacks must never allocate",
        before.0, after.0, before.1, after.1
    );
}

use std::sync::Arc;

use cpal::SampleFormat;
use music_player_rs::audio::format::BitDepth;
use music_player_rs::audio::output::{dop_render_for, pcm_render_for, RawRender};
use music_player_rs::audio::render::gain::{AtomicGain, GainStage, NoGain};
use music_player_rs::audio::render::pcm::PcmSample;
use music_player_rs::audio::render::tpdf::Tpdf;
use music_player_rs::audio::render::{RenderCore, RingSample};
use music_player_rs::audio::session::{RingPayload, SessionShared};

const OUT_CH: usize = 2;
const RING_CAP_SAMPLES: usize = 8192;
const CALLBACK_FRAMES: usize = 512;
const PRIME_FRAMES: usize = 1024;

const EXACT_24: RingPayload = RingPayload::ExactI32 {
    valid_bits: match BitDepth::new(24) {
        Some(bits) => bits,
        None => panic!("24 бита — допустимая разрядность"),
    },
};

/// Состояние колбэка, в котором меряется рендер (§6.14–§6.17).
#[derive(Debug, Clone, Copy)]
enum State {
    /// Воспроизведение, ring полон на много периодов.
    Playing,
    /// Ring меньше, чем съедят вызовы: частичное чтение + тишина + underrun.
    Underrun,
    /// `playing == false`: тишина без чтения ring.
    Paused,
    /// Ring ниже порога priming, конец трека неизвестен: тишина.
    Priming,
    /// Декодер подтвердил остановку: фаза 3 seek (сброс ring) и тишина.
    Seek,
}

const STATES: [State; 5] = [
    State::Playing,
    State::Underrun,
    State::Paused,
    State::Priming,
    State::Seek,
];

/// Ring сэмплов `value`, заполненный под состояние, и ядро рендера над ним.
fn core_for<P: RingSample>(state: State, value: P) -> (RenderCore<P>, Arc<SessionShared>) {
    let shared = Arc::new(SessionShared::new());
    let fill_frames = match state {
        State::Underrun => CALLBACK_FRAMES / 4,
        State::Priming => PRIME_FRAMES / 2,
        _ => RING_CAP_SAMPLES / OUT_CH,
    };
    let (mut producer, consumer) = rtrb::RingBuffer::<P>::new(RING_CAP_SAMPLES);
    for _ in 0..fill_frames * OUT_CH {
        assert!(producer.push(value).is_ok());
    }
    shared.playing.store(!matches!(state, State::Paused), Ordering::Release);
    let prime = match state {
        State::Priming => PRIME_FRAMES,
        _ => 0,
    };
    if let State::Seek = state {
        let generation = shared.request_seek(1000);
        shared.decoder_stopped.store(generation, Ordering::Release);
    }
    // Рукопожатие seek после фазы 3 оставляем незавершённым: каждое
    // следующее поколение снова уводит колбэк в ветку сброса.
    (RenderCore::new(consumer, Arc::clone(&shared), OUT_CH, prime), shared)
}

/// Прогрев (первый вызов) и замер `iterations` вызовов рендера.
fn check_render(name: &str, render: &mut dyn RawRender, out: &mut [u8], seek: Option<&SessionShared>) {
    render.render(out);
    assert_zero_alloc(name, 300, || {
        if let Some(shared) = seek {
            let generation = shared.request_seek(1000);
            shared.decoder_stopped.store(generation, Ordering::Release);
        }
        render.render(out);
    });
}

// Тестовый код вне #[test]: отказ сборки рендера — провал теста (AM1.0 §6.29).
#[allow(clippy::expect_used)]
fn check_pcm<P: PcmSample, G: GainStage>(ring: &str, value: P, payload: RingPayload) {
    let formats = [
        SampleFormat::I16,
        SampleFormat::I24,
        SampleFormat::I32,
        SampleFormat::F32,
    ];
    let mut out = vec![0u8; CALLBACK_FRAMES * OUT_CH * 4];
    for format in formats {
        for dither in [None, Some(1u32)] {
            for state in STATES {
                let (core, shared) = core_for(state, value);
                let tpdf = dither.map_or_else(Tpdf::off, Tpdf::with_seed);
                let mut render = pcm_render_for::<P, G>(format, core, payload, tpdf)
                    .expect("PCM-рендер для формата");
                let len = CALLBACK_FRAMES * OUT_CH * format.sample_size();
                let name = format!(
                    "PcmRender<{ring}, {}> {format:?} dither={dither:?} {state:?}",
                    std::any::type_name::<G>()
                );
                let seek = matches!(state, State::Seek).then_some(&*shared);
                check_render(&name, render.as_mut(), &mut out[..len], seek);
            }
        }
    }
}

// Тестовый код вне #[test]: отказ сборки рендера — провал теста (AM1.0 §6.29).
#[allow(clippy::expect_used)]
fn check_dop() {
    let mut out = vec![0u8; CALLBACK_FRAMES * OUT_CH * 4];
    for format in [SampleFormat::I24, SampleFormat::I32] {
        for state in STATES {
            let (core, shared) = core_for(state, 0x0069_6900_i32);
            let mut render = dop_render_for(format, core).expect("DoP-рендер для формата");
            let len = CALLBACK_FRAMES * OUT_CH * format.sample_size();
            let name = format!("DopRender {format:?} {state:?}");
            let seek = matches!(state, State::Seek).then_some(&*shared);
            check_render(&name, render.as_mut(), &mut out[..len], seek);
        }
    }
}

/// Один `#[test]`: счётчики аллокатора глобальны для процесса, а параллельные
/// тесты загрязнили бы окно замера чужими аллокациями.
#[test]
fn callback_zero_alloc_all_formats_and_states() {
    check_pcm::<i32, NoGain>("i32", 0x1234_5600, EXACT_24);
    check_pcm::<i32, AtomicGain>("i32", 0x1234_5600, EXACT_24);
    check_pcm::<f32, NoGain>("f32", 0.25, RingPayload::F32);
    check_pcm::<f32, AtomicGain>("f32", 0.25, RingPayload::F32);
    check_dop();
}
