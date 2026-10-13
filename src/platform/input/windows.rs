//! Направление прокрутки тачпада Windows (§6.11 «Направление тачпада», ТЗ-36,
//! ТЗ-54 п. 3): системная настройка высокоточного тачпада читается из реестра
//! — `HKEY_CURRENT_USER`, ключ
//! `Software\Microsoft\Windows\CurrentVersion\PrecisionTouchPad`, значение
//! `ScrollDirection` (REG_DWORD). `0` — движение пальцев вверх приходит с
//! отрицательным знаком, `0xFFFF_FFFF` — с положительным. Файл собирается
//! только на Windows.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::ffi::c_void;
use std::ptr::null_mut;

use windows_sys::core::{w, PCWSTR};
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

use super::TouchpadDirection;

/// Ключ настроек высокоточного тачпада в `HKEY_CURRENT_USER` (§6.11): UTF-16
/// с завершающим нулём, собирается при компиляции.
const SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\PrecisionTouchPad");

/// Имя значения с направлением прокрутки (§6.11).
const VALUE: PCWSTR = w!("ScrollDirection");

/// `ScrollDirection`: движение пальцев вверх приходит с отрицательным знаком.
const SCROLL_UP_NEGATIVE: u32 = 0;

/// `ScrollDirection`: движение пальцев вверх приходит с положительным знаком.
const SCROLL_UP_POSITIVE: u32 = 0xFFFF_FFFF;

/// Размер REG_DWORD в байтах: столько `RegGetValueW` обязан записать.
const DWORD_SIZE: u32 = u32::BITS / 8;

/// Системное направление прокрутки тачпада (§6.11 «Направление тачпада»,
/// ТЗ-36, ТЗ-54 п. 3). Ошибка — значение не прочитано или не распознано:
/// текст идёт в журнал, знак событий тачпада тогда не исправляется.
pub(super) fn touchpad_direction() -> Result<TouchpadDirection, Box<str>> {
    let mut data: u32 = 0;
    let mut size: u32 = DWORD_SIZE;
    // SAFETY: `SUBKEY` и `VALUE` — статические строки UTF-16 с завершающим
    // нулём; `data` и `size` живут до конца вызова, `size` равен размеру
    // буфера `data` в байтах; тип значения не запрашивается (`null_mut`), а
    // флаг `RRF_RT_REG_DWORD` не даёт системе записать больше четырёх байт.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            SUBKEY,
            VALUE,
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut data as *mut u32 as *mut c_void,
            &mut size,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!("ScrollDirection не прочитан: код ошибки {status}").into());
    }
    if size != DWORD_SIZE {
        return Err(format!("ScrollDirection: размер значения {size} байт вместо {DWORD_SIZE}")
            .into());
    }
    direction_from_value(data)
}

/// Значение `ScrollDirection` → направление (§6.11 «Направление тачпада»,
/// ТЗ-36). Любое значение, кроме двух известных, — ошибка.
fn direction_from_value(value: u32) -> Result<TouchpadDirection, Box<str>> {
    match value {
        SCROLL_UP_NEGATIVE => Ok(TouchpadDirection::UpNegative),
        SCROLL_UP_POSITIVE => Ok(TouchpadDirection::UpPositive),
        other => Err(format!("ScrollDirection: значение 0x{other:08X} не распознано").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_from_value_maps_known_values() {
        assert_eq!(direction_from_value(0), Ok(TouchpadDirection::UpNegative));
        assert_eq!(direction_from_value(0xFFFF_FFFF), Ok(TouchpadDirection::UpPositive));
        let err = direction_from_value(1).unwrap_err();
        assert_eq!(&*err, "ScrollDirection: значение 0x00000001 не распознано");
    }

    /// На машине без высокоточного тачпада (CI) значения нет: допустимы оба
    /// исхода, при ошибке текст не пуст.
    #[test]
    fn touchpad_direction_reads_registry_without_panic() {
        let result = touchpad_direction();
        if let Err(text) = result {
            assert!(!text.is_empty());
        }
    }
}
