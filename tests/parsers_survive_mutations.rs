//! Фаззинг разборщиков DSF/DFF без новых зависимостей (AM1.0 §6.29; ТЗ-92,
//! И-Р15): 10 000 итераций на разборщик с фиксированным зерном xorshift —
//! случайная замена байтов, обрезка и раздувание полей размеров в эталонных
//! заголовках `tests/data/`. Условия: нет паники; ошибка — только
//! `FileError::Corrupt` или `Unsupported`; пиковая память итерации
//! ≤ `file_len + 16 МиБ`.
//!
//! `APAP_FUZZ_ITERS` только увеличивает число итераций (долгий ручной прогон
//! перед приёмкой ТЗ-92); без переменной тест не пропускается (ТЗ-114).
//!
//! Отдельный бинарник теста: `#[global_allocator]` здесь считает память только
//! этого теста.

use music_player_rs::audio::dsd::parse_container_header;
use music_player_rs::audio::error::FileError;
use music_player_rs::audio::format::Container;
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct PeakAllocator;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(size: usize) {
    let now = CURRENT.fetch_add(size, Ordering::SeqCst) + size;
    PEAK.fetch_max(now, Ordering::SeqCst);
}

unsafe impl GlobalAlloc for PeakAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grow(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
        grow(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: PeakAllocator = PeakAllocator;

/// Постоянный запас памяти на трек сверх длины файла (§6.29).
const RESERVE: usize = 16 << 20;
const DEFAULT_ITERS: u64 = 10_000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n.max(1)).unwrap_or(1)).unwrap_or(0)
    }
}

fn iterations() -> u64 {
    std::env::var("APAP_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.replace('_', "").parse::<u64>().ok())
        .map_or(DEFAULT_ITERS, |n| n.max(DEFAULT_ITERS))
}

fn reference(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Смещения 8-байтных полей размеров: DSF — фиксированные; DFF — после каждого
/// идентификатора чанка. Раздувание этих полей проверяет checked-арифметику.
fn size_fields(bytes: &[u8], container: Container) -> Vec<(usize, usize)> {
    match container {
        Container::Dsf => vec![(4, 8), (12, 8), (20, 8), (32, 8), (52, 4), (64, 8), (72, 4), (84, 8)],
        _ => {
            let ids: [&[u8]; 7] = [b"FRM8", b"FVER", b"PROP", b"FS  ", b"CHNL", b"CMPR", b"DSD "];
            let head = &bytes[..bytes.len().min(256)];
            head.windows(4)
                .enumerate()
                .filter(|(_, w)| ids.contains(w))
                .map(|(i, _)| (i + 4, 8))
                .filter(|(at, n)| at + n <= bytes.len())
                .collect()
        }
    }
}

fn mutate(rng: &mut Rng, base: &[u8], fields: &[(usize, usize)]) -> Vec<u8> {
    let mut b = base.to_vec();
    match rng.below(3) {
        0 => {
            // Замена 1–8 байтов в заголовочной части.
            for _ in 0..=rng.below(8) {
                let i = rng.below(b.len().min(512));
                b[i] = u8::try_from(rng.next() & 0xFF).unwrap_or(0);
            }
        }
        1 => b.truncate(rng.below(b.len())),
        _ => {
            let (at, n) = fields[rng.below(fields.len())];
            let v: u64 = match rng.below(4) {
                0 => u64::MAX,
                1 => u64::MAX - u64::try_from(rng.below(16)).unwrap_or(0),
                2 => rng.next(),
                _ => u64::try_from(base.len()).unwrap_or(0) + u64::try_from(rng.below(1 << 20)).unwrap_or(0),
            };
            let bytes = if matches!(base.get(0..4), Some(b"FRM8")) { v.to_be_bytes() } else { v.to_le_bytes() };
            let off = if matches!(base.get(0..4), Some(b"FRM8")) { 8 - n } else { 0 };
            b[at..at + n].copy_from_slice(&bytes[off..off + n]);
        }
    }
    b
}

fn fuzz(name: &str, container: Container) {
    let base = reference(name);
    let fields = size_fields(&base, container);
    assert!(!fields.is_empty(), "{name}: нет полей размеров");
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ u64::try_from(base.len()).unwrap_or(0));
    let mut errors = 0u64;
    for iter in 0..iterations() {
        let bytes = mutate(&mut rng, &base, &fields);
        let file_len = bytes.len();
        let mut cursor = Cursor::new(bytes);
        let before = CURRENT.load(Ordering::SeqCst);
        PEAK.store(before, Ordering::SeqCst);
        let result = parse_container_header(&mut cursor, container);
        let used = PEAK.load(Ordering::SeqCst).saturating_sub(before);
        assert!(used <= file_len + RESERVE, "{name} #{iter}: {used} байт при файле {file_len}");
        match result {
            Ok(()) => {}
            Err(FileError::Corrupt(_) | FileError::Unsupported { .. }) => errors += 1,
            Err(other) => panic!("{name} #{iter}: неожиданная ошибка {other:?}"),
        }
    }
    assert!(errors > 0, "{name}: мутации не дали ни одной ошибки — фаззинг не работает");
}

#[test]
fn parsers_survive_mutations() {
    fuzz("dsf_dsd64_1k.dsf", Container::Dsf);
    fuzz("dff_dsd64_1k.dff", Container::Dff);
}
