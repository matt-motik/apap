//! Machine-checkable proof of spec_visualizer_v5.1.md §11.1's FullTrackWorker
//! budget row "Peak RAM при построении (nfft=8192, max_frames=4000) ≤ 128 МБ"
//! and, structurally, of "Переполнение памяти (DSD512): Плавная децимация на
//! лету" (§ risk table) — i.e. that `Envelope`/`Spectrogram` never allocate
//! proportionally to track length, which is *why* a DSD512 track can't OOM
//! them regardless of how many samples get fed through.
//!
//! Two kinds of proof, both against the real production types:
//!   1. Construction cost is independent of `total_frames` — compare a
//!      normal-length track against one ~40x longer than any realistic
//!      DSD512 5-minute frame count and assert identical peak bytes.
//!   2. `feed()` is zero-allocation per call — proven the same way as
//!      tests/rt_zero_alloc.rs — which is what makes streaming an arbitrary
//!      number of chunks safe without the peak growing with input volume.
//!
//! Single #[test] fn for the same reason as the other allocator-instrumented
//! tests here: parallel #[test] fns would share and pollute the process-wide
//! counters.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct TrackingAllocator;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static DEALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static CURRENT_BYTES: AtomicUsize = AtomicUsize::new(0);
/// High-water mark of CURRENT_BYTES since the last `reset_window`.
static WINDOW_PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        let cur = CURRENT_BYTES.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
        WINDOW_PEAK_BYTES.fetch_max(cur, Ordering::SeqCst);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        CURRENT_BYTES.fetch_sub(layout.size(), Ordering::SeqCst);
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() {
            if new_size >= layout.size() {
                let delta = new_size - layout.size();
                let cur = CURRENT_BYTES.fetch_add(delta, Ordering::SeqCst) + delta;
                WINDOW_PEAK_BYTES.fetch_max(cur, Ordering::SeqCst);
            } else {
                CURRENT_BYTES.fetch_sub(layout.size() - new_size, Ordering::SeqCst);
            }
        }
        new_ptr
    }
}

#[global_allocator]
static GLOBAL: TrackingAllocator = TrackingAllocator;

fn alloc_dealloc_counts() -> (usize, usize) {
    (ALLOC_COUNT.load(Ordering::SeqCst), DEALLOC_COUNT.load(Ordering::SeqCst))
}

/// Start a peak-tracking window: subsequent allocations' contribution to the
/// high-water mark is measured relative to the current baseline.
fn start_window() -> usize {
    let baseline = CURRENT_BYTES.load(Ordering::SeqCst);
    WINDOW_PEAK_BYTES.store(baseline, Ordering::SeqCst);
    baseline
}

/// Peak bytes allocated above the baseline since `start_window`.
fn window_peak_added(baseline: usize) -> usize {
    WINDOW_PEAK_BYTES.load(Ordering::SeqCst).saturating_sub(baseline)
}

use music_player_rs::audio::fulltrack::Envelope;
use music_player_rs::audio::spectrogram::Spectrogram;
use music_player_rs::audio::visualizer::SpectrogramCfg;

/// spec_visualizer_v5.1.md §11.1: "Peak RAM при построении ... ≤ 128 МБ".
const PEAK_RAM_BUDGET_BYTES: usize = 128 * 1024 * 1024;

/// A normal 5-minute track at a typical PCM rate.
const NORMAL_TRACK_FRAMES: u64 = 44_100 * 60 * 5;

/// Deliberately far beyond any realistic DSD512-visualization frame count
/// (DSD512 raw rate is ~22.6 MHz; even without any decimation, 5 minutes is
/// ~6.8 billion frames — this constant is chosen larger than that on purpose,
/// as a worst-case stress value, not a claim about the real decimated rate).
const HUGE_DSD512_SCALE_FRAMES: u64 = 8_000_000_000;

const ENVELOPE_COLUMNS: usize = 4000; // spec's own "max_frames=4000" parenthetical
const ENVELOPE_CHANNELS: usize = 2;

#[test]
fn fulltrack_builders_stay_within_memory_budget() {
    // ---- Envelope: construction cost must not depend on total_frames ----
    let baseline = start_window();
    let normal = Envelope::new(NORMAL_TRACK_FRAMES, ENVELOPE_CHANNELS, ENVELOPE_COLUMNS);
    let normal_peak = window_peak_added(baseline);
    drop(normal);

    let baseline = start_window();
    let huge = Envelope::new(HUGE_DSD512_SCALE_FRAMES, ENVELOPE_CHANNELS, ENVELOPE_COLUMNS);
    let huge_peak = window_peak_added(baseline);

    assert_eq!(
        normal_peak, huge_peak,
        "Envelope::new's peak allocation depends on total_frames \
         (normal 5-min track: {normal_peak} bytes vs a DSD512-scale track: {huge_peak} bytes) \
         — it must depend only on columns*channels, or long tracks risk OOM"
    );
    assert!(
        huge_peak <= PEAK_RAM_BUDGET_BYTES,
        "Envelope::new peak allocation {huge_peak} bytes exceeds the \
         spec_visualizer_v5.1.md §11.1 budget of {PEAK_RAM_BUDGET_BYTES} bytes"
    );

    // ---- Envelope::feed must be zero-alloc (proves streaming is O(1) per
    // chunk regardless of how many chunks a long track requires) ----
    let mut env = huge;
    let chunk = vec![0.1f32; 4096 * ENVELOPE_CHANNELS];
    env.feed(&chunk, ENVELOPE_CHANNELS); // warm-up
    let before = alloc_dealloc_counts();
    for _ in 0..500 {
        env.feed(&chunk, ENVELOPE_CHANNELS);
    }
    let after = alloc_dealloc_counts();
    assert_eq!(
        before, after,
        "Envelope::feed allocated during 500 calls (alloc {}->{}, dealloc {}->{})",
        before.0, after.0, before.1, after.1
    );
    drop(env);

    // ---- Spectrogram: same two proofs ----
    let cfg = SpectrogramCfg::default();

    let baseline = start_window();
    let normal = Spectrogram::new(NORMAL_TRACK_FRAMES, 44_100, 2, false, &cfg)
        .expect("Spectrogram::new should succeed for a normal track");
    let normal_peak = window_peak_added(baseline);
    drop(normal);

    let baseline = start_window();
    let mut huge = Spectrogram::new(HUGE_DSD512_SCALE_FRAMES, 44_100, 2, true, &cfg)
        .expect("Spectrogram::new should succeed for a DSD512-scale track");
    let huge_peak = window_peak_added(baseline);

    // Unlike Envelope (whose buffer size is a direct function of the
    // caller-chosen columns/channels only), Spectrogram derives its column
    // count from `total_frames` via `adaptive_plan` — a ~600x longer track
    // can legitimately saturate at a slightly wider image (more columns up
    // to `max_frames`) before hop growth caps it, so exact equality isn't
    // the right invariant here. The property that actually matters is: peak
    // memory must stay within budget and must NOT scale proportionally with
    // track length (a real "no decimation" bug would blow past the budget
    // by orders of magnitude for a 600x frame-count increase, not ~2x).
    let growth_ratio = huge_peak as f64 / normal_peak.max(1) as f64;
    assert!(
        growth_ratio <= 10.0,
        "Spectrogram::new peak allocation grew {growth_ratio:.1}x for a \
         {}x longer track (normal: {normal_peak} bytes, DSD512-scale: {huge_peak} bytes) \
         — adaptive_plan's column/hop decimation should keep this roughly flat, not proportional",
        HUGE_DSD512_SCALE_FRAMES / NORMAL_TRACK_FRAMES
    );
    assert!(
        huge_peak <= PEAK_RAM_BUDGET_BYTES,
        "Spectrogram::new peak allocation {huge_peak} bytes exceeds the \
         spec_visualizer_v5.1.md §11.1 budget of {PEAK_RAM_BUDGET_BYTES} bytes"
    );

    let chunk = vec![0.1f32; 4096 * 2];
    huge.feed(&chunk, 2); // warm-up (also settles the fft_scratch buffer's first use)
    let before = alloc_dealloc_counts();
    for _ in 0..500 {
        huge.feed(&chunk, 2);
    }
    let after = alloc_dealloc_counts();
    assert_eq!(
        before, after,
        "Spectrogram::feed allocated during 500 calls (alloc {}->{}, dealloc {}->{}) — \
         this is exactly what Fft::process() (vs process_with_scratch) would cause",
        before.0, after.0, before.1, after.1
    );
}
