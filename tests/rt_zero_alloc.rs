//! Machine-checkable proof of docs/spec_audio_core_v2.0.md §3.4 ("Тест-детектор
//! аллокаций в колбэке" — a hard RT requirement, §8 budget table: "Аллокации в
//! колбэке: 0, жёсткое требование") and §11.1 of spec_visualizer_v5.1.md
//! ("Аллокации в горячем цикле: 0" for the LiveWorker path).
//!
//! This is a *separate* Cargo integration test binary (each file under
//! tests/ is its own crate root), so the `#[global_allocator]` declared here
//! only instruments this test binary — it never touches the real app binary
//! or `cargo build`.
//!
//! Every RT callback variant is exercised through every branch it has
//! (playing/not-playing, full ring, underrun, pending-seek) because the
//! zero-allocation contract has to hold on *every* path, not just the happy
//! one — that's the whole point of measuring instead of trusting a review.

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

use music_player_rs::audio::player::{
    audio_callback_f32_rt, audio_callback_i16_rt, audio_callback_i32_dop_rt,
    audio_callback_i32_pcm_rt, audio_callback_u8_rt,
};
use music_player_rs::audio::worker::{RtConsumer, RtShared};

const OUT_RATE: u32 = 44_100;
const OUT_CH: usize = 2;
const RING_CAP_SAMPLES: usize = 8192;
const CALLBACK_FRAMES: usize = 512;

/// A consumer wired to a ring pre-filled with `fill_frames` frames of dummy
/// PCM and marked playing — the "steady state" a real cpal callback sees.
fn playing_consumer(fill_frames: usize) -> RtConsumer {
    let shared = RtShared::new(OUT_RATE, OUT_CH, false);
    let (mut producer, ring) = rtrb::RingBuffer::<f32>::new(RING_CAP_SAMPLES);
    for i in 0..(fill_frames * OUT_CH).min(RING_CAP_SAMPLES) {
        let _ = producer.push(((i % 200) as f32 / 200.0) - 0.5);
    }
    shared.set_playing(true);
    shared.set_finished(false);
    RtConsumer::new(ring, shared)
}

fn idle_consumer() -> RtConsumer {
    let shared = RtShared::new(OUT_RATE, OUT_CH, false);
    let (_producer, ring) = rtrb::RingBuffer::<f32>::new(RING_CAP_SAMPLES);
    // playing left false: exercises the early-return silence branch.
    RtConsumer::new(ring, shared)
}

/// A consumer with a seek published but not yet acknowledged by the worker
/// — exercises `reconcile_seek`'s "not ready yet" branch.
fn pending_seek_consumer(fill_frames: usize) -> RtConsumer {
    let consumer = playing_consumer(fill_frames);
    consumer.shared().begin_seek(1000);
    consumer
}

// All checks run from a single #[test] (see below) rather than one #[test]
// per callback: cargo test runs #[test] fns in parallel threads by default,
// and ALLOC_COUNT/DEALLOC_COUNT are process-global — concurrent tests would
// pollute each other's measurement window with unrelated allocations. A
// single test function serializes everything onto one thread, which is the
// only way this counter is meaningful.
macro_rules! zero_alloc_check {
    ($fn_name:ident, $callback:ident, $sample_ty:ty, $fill:expr) => {
        fn $fn_name() {
            let mut data = vec![<$sample_ty>::default(); CALLBACK_FRAMES * OUT_CH];

            // Steady-state playing, ring full enough for many calls.
            let mut c = playing_consumer($fill);
            $callback(&mut c, &mut data); // warm-up: first branch dispatch, cache lines, etc.
            assert_zero_alloc(concat!(stringify!($callback), " (playing, steady)"), 500, || {
                $callback(&mut c, &mut data);
            });

            // Ring smaller than what 500 calls would consume: forces the
            // underrun/partial-pull-then-silence-fill branch repeatedly.
            let mut c = playing_consumer(CALLBACK_FRAMES / 4);
            $callback(&mut c, &mut data);
            assert_zero_alloc(concat!(stringify!($callback), " (underrun)"), 500, || {
                $callback(&mut c, &mut data);
            });

            // Not playing: early-return silence-fill branch.
            let mut c = idle_consumer();
            $callback(&mut c, &mut data);
            assert_zero_alloc(concat!(stringify!($callback), " (idle)"), 200, || {
                $callback(&mut c, &mut data);
            });

            // Seek published, worker hasn't acked yet: reconcile_seek's
            // false branch (drain + silence-fill).
            let mut c = pending_seek_consumer($fill);
            $callback(&mut c, &mut data);
            assert_zero_alloc(concat!(stringify!($callback), " (pending seek)"), 200, || {
                $callback(&mut c, &mut data);
            });
        }
    };
}

zero_alloc_check!(check_f32, audio_callback_f32_rt, f32, RING_CAP_SAMPLES / OUT_CH);
zero_alloc_check!(check_i16, audio_callback_i16_rt, i16, RING_CAP_SAMPLES / OUT_CH);
zero_alloc_check!(check_u8, audio_callback_u8_rt, u8, RING_CAP_SAMPLES / OUT_CH);
zero_alloc_check!(check_i32_pcm, audio_callback_i32_pcm_rt, i32, RING_CAP_SAMPLES / OUT_CH);
zero_alloc_check!(check_i32_dop, audio_callback_i32_dop_rt, i32, RING_CAP_SAMPLES / OUT_CH);

/// Single test: see the note on `zero_alloc_check!` above for why this
/// isn't five separate #[test] fns.
#[test]
fn rt_callbacks_are_zero_alloc() {
    check_f32();
    check_i16();
    check_u8();
    check_i32_pcm();
    check_i32_dop();
}
