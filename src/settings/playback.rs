//! Опции режимов воспроизведения (AM1.0 §2.2, ТЗ-93…ТЗ-131).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use crate::audio::backend::SharedDeviceId;

/// Режим вывода (ADR-04, §2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ModeKind {
    #[default]
    Compatible,
    Optimal,
    Strict,
}

/// Выбор устройства Совместимого режима: системное по умолчанию или именованное
/// Shared-устройство (§2.2).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum SharedDeviceChoice {
    #[default]
    SystemDefault,
    Named(SharedDeviceId),
}

/// ОВ-3. Значение по умолчанию — SameFamily (§2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RateFallbackRule {
    #[default]
    SameFamily,
    Nearest,
    NoDownsample,
}

/// ОВС-7 (заменяет ОВ-8): фильтры SRC по образцу меню ЦАП, все с линейной фазой
/// и подавлением алиасинга ≥ 150 дБ. «Крутой, короткая задержка» и «Медленный,
/// короткая задержка» (минимальная фаза) непредставимы; UI показывает их из
/// дескриптора как NotImplemented (ТЗ-130, И-Т19, §2.2).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum SrcFilter {
    #[default]
    Steep,
    Slow,
    VerySlow,
}

/// ОВС-6: начало полосы подавления финального фильтра DSD→PCM; по умолчанию
/// 30 кГц (§2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DsdFilter {
    K24,
    #[default]
    K30,
    K50,
}

/// ОВС-13: буфер устройства в Exclusive; 4 периода в буфере (ТЗ-131, §2.2).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum DeviceBuffer {
    Ms20,
    #[default]
    Ms40,
    Ms100,
    Ms200,
    Ms400,
}

impl DeviceBuffer {
    /// Размер буфера устройства в миллисекундах (ОВС-13, ТЗ-131).
    pub const fn ms(self) -> u16 {
        match self {
            DeviceBuffer::Ms20 => 20,
            DeviceBuffer::Ms40 => 40,
            DeviceBuffer::Ms100 => 100,
            DeviceBuffer::Ms200 => 200,
            DeviceBuffer::Ms400 => 400,
        }
    }

    /// Длительность одного периода: буфер из 4 периодов (ОВС-13).
    pub const fn period_ms(self) -> u16 {
        self.ms() / 4
    }
}

/// ОВ-7: только два значения (§2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Dither {
    #[default]
    Tpdf,
    Off,
}

/// Решение 6: единственное реализуемое значение. «Ремодуляция» непредставима
/// и показывается в UI из дескриптора как NotImplemented (ТЗ-34, §2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DsdAboveDac {
    #[default]
    ConvertToPcm,
}

/// Громкость в процентах, 0..=100. Конструктор проверяет диапазон (ТЗ-129,
/// И-Т15, §2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Volume(u8);

impl Volume {
    pub const fn new(v: u8) -> Option<Volume> {
        if v <= 100 {
            Some(Volume(v))
        } else {
            None
        }
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

/// Размер буфера декодирование→вывод, 100..=10 000 мс (матрица §5, ТЗ-129,
/// И-Т15, §2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BufferMs(u16);

impl BufferMs {
    pub const fn new(v: u16) -> Option<BufferMs> {
        if v >= 100 && v <= 10_000 {
            Some(BufferMs(v))
        } else {
            None
        }
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §7.2: пять значений `DeviceBuffer`, по умолчанию 40 мс; период = буфер/4.
    #[test]
    fn device_buffer_values_and_default() {
        assert_eq!(DeviceBuffer::default(), DeviceBuffer::Ms40);
        let values = [
            (DeviceBuffer::Ms20, 20u16),
            (DeviceBuffer::Ms40, 40u16),
            (DeviceBuffer::Ms100, 100u16),
            (DeviceBuffer::Ms200, 200u16),
            (DeviceBuffer::Ms400, 400u16),
        ];
        for (buffer, ms) in values {
            assert_eq!(buffer.ms(), ms);
            assert_eq!(buffer.period_ms(), ms / 4);
        }
    }

    #[test]
    fn volume_rejects_out_of_range() {
        assert_eq!(Volume::new(0).map(Volume::get), Some(0));
        assert_eq!(Volume::new(100).map(Volume::get), Some(100));
        assert_eq!(Volume::new(101), None);
    }

    #[test]
    fn buffer_ms_rejects_out_of_range() {
        assert_eq!(BufferMs::new(100).map(BufferMs::get), Some(100));
        assert_eq!(BufferMs::new(10_000).map(BufferMs::get), Some(10_000));
        assert_eq!(BufferMs::new(99), None);
        assert_eq!(BufferMs::new(10_001), None);
    }
}
