//! Бейдж bit-perfect (AM1.0 §2.5, ADR-06).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

/// Причина «Не bit-perfect». Порядок вариантов — приоритет показа: первая
/// причина становится основной, остальные идут в признак «+N» (ADR-06, ТЗ-53, §2.5).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum NbpReason {
    /// Тест тракта не пройден.
    TestFailed,
    /// Драйвер не подтвердил запрошенные параметры (`hw_params`).
    DriverParamsUnconfirmed,
    /// DSD-источник выведен как PCM.
    DsdToPcm,
    /// Ресемплинг (SRC).
    Resample,
    /// Сведение каналов.
    Downmix,
    /// Усечение разрядности (вкл. float64 → float32, ОВС-1).
    Truncation,
    /// Квантование float-источника в целое (ОВС-1).
    FloatToInt,
    /// 32-битный целый источник переведён во float.
    Int32ToFloat,
    /// Программный mute стадии усиления.
    Mute,
    /// Программная громкость ≠ 100 %.
    Volume,
    /// Применён дизеринг.
    Dither,
}
