//! Machine-checkable proof of spec_visualizer_v5.1.md §11.1 ("Мгновенные
//! (LiveWorker): Аллокации в горячем цикле: 0") for `SpectrumEngine::consume`
//! — the per-frame FFT/band-power hot path that runs on the LiveWorker
//! thread while audio plays.
//!
//! Same technique and same reason for a single #[test] fn as
//! tests/rt_zero_alloc.rs: a separate integration-test binary with its own
//! #[global_allocator], one sequential test (parallel #[test] fns would
//! share and pollute the global counters).

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

use music_player_rs::audio::analyzer::SpectrumEngine;
use music_player_rs::audio::visualizer::SpectrumCfg;

const RATE: u32 = 44_100;
const IN_CH: usize = 2;
/// Large enough to force many FFT windows (SPECTRUM_HOP-sized chunks) per
/// `consume` call, well past whatever `compute_bands`'s `out.reserve` needs
/// to settle its capacity.
const RING_CAP_SAMPLES: usize = 1 << 16;

#[test]
fn spectrum_engine_consume_is_zero_alloc_after_warmup() {
    let cfg = SpectrumCfg::default();
    let mut engine = SpectrumEngine::new(&cfg, RATE, IN_CH);

    let (mut producer, mut ring) = rtrb::RingBuffer::<f32>::new(RING_CAP_SAMPLES);
    let fill = |producer: &mut rtrb::Producer<f32>| {
        for i in 0..RING_CAP_SAMPLES {
            let _ = producer.push(((i % 997) as f32 / 997.0) - 0.5);
        }
    };

    // Warm-up: let `out`'s Vec capacity settle (compute_bands does
    // out.clear() + out.reserve(bands*proc_ch), which only allocates the
    // first time capacity is insufficient) and exercise the ring-refill
    // path once before measuring.
    let mut out = Vec::new();
    fill(&mut producer);
    engine.consume(&mut ring, &cfg, &mut out);

    let before = counts();
    for _ in 0..50 {
        fill(&mut producer);
        engine.consume(&mut ring, &cfg, &mut out);
    }
    let after = counts();
    assert_eq!(
        before, after,
        "SpectrumEngine::consume allocated during the LiveWorker hot loop \
         (alloc {}->{}, dealloc {}->{}) — spec_visualizer_v5.1.md §11.1 requires 0",
        before.0, after.0, before.1, after.1
    );

    // Also cover the "ring empty, nothing to produce" branch (pull_frame
    // returns None immediately) — must be equally allocation-free.
    let before = counts();
    for _ in 0..50 {
        engine.consume(&mut ring, &cfg, &mut out);
    }
    let after = counts();
    assert_eq!(
        before, after,
        "SpectrumEngine::consume allocated while starved of input (alloc {}->{}, dealloc {}->{})",
        before.0, after.0, before.1, after.1
    );
}
