# Дополнение к архитектурной спецификации APAP v5.0

## Advanced Preferences / «Дополнительно»

**Дата:** 2026-09-22  
**Базовая спецификация:** `APAP_architecture_spec_v5.0_PlaybackPolicy_PathPlanner_SignalPath_BackendEndpoint.md`

## 1. Архитектурное решение

Раздел **«Дополнительно»** сохраняется.

Он не является отдельной системой выбора backend и не может обходить:

```text
PlaybackPolicy
      ↓
PathPlanner
      ↓
SignalPath
      ↓
BackendEndpoint
```

Полная модель:

```text
SourceFormat
DeviceCapabilities
PlaybackPolicy
AdvancedPreferences
        │
        ▼
   PathPlanner
        │
        ▼
     PathPlan
      /    \
SignalPath  BackendEndpoint
```

`PlaybackPolicy` задаёт **hard constraints**.  
`AdvancedPreferences` задаёт **preferences / additional constraints**.

## 2. Формальное правило

```text
AllowedPaths =
    PolicyAllowedPaths
    ∩ DeviceSupportedPaths
    ∩ UserAllowedPaths
```

После этого Planner выбирает наиболее предпочтительный путь.

AdvancedPreferences не могут расширить множество путей, запрещённых Policy.

## 3. PlaybackPolicy

### Shared

Гарантируется:

```text
BackendAccess = Shared
BackendEndpoint = Shared(...)
```

Raw `hw:*` не допускается.

Даже `PreferExclusive` в AdvancedPreferences не может нарушить Shared policy.

### BestPossible

Planner ищет лучший допустимый путь. Preferences могут:

- запрещать отдельные преобразования;
- задавать порядок предпочтений;
- ограничивать DSD paths;
- задавать предпочтение частоты;
- задавать совместимые предпочтения backend access.

### Strict

Гарантируется:

```text
ConversionPlan::None
```

Допускаются только:

```text
BitPerfectPcm
NativeDsd
DoP
```

AdvancedPreferences не могут разрешить SRC, sample-format conversion, channel conversion или DSD→PCM.

## 4. Рекомендуемая модель AdvancedPreferences

```rust
pub struct AdvancedPreferences {
    pub access: AccessPreference,
    pub pcm: PcmPreferences,
    pub sample_rate: SampleRatePreference,
    pub dsd: DsdPathPreference,
    pub dop: DopPreference,
}
```

Конкретные enum/struct могут быть уточнены при реализации.

### AccessPreference

```text
Automatic
PreferShared
PreferExclusive
```

Это preference, а не прямой backend command.

### PCM

```rust
pub struct PcmPreferences {
    pub allow_resampling: bool,
    pub allow_sample_format_conversion: bool,
    pub allow_channel_conversion: bool,
}
```

### Sample rate

```text
SourceIfPossible
BestSupported
MaximumSupported
```

### DSD

Например:

```text
NativeThenDopThenPcm
NativeThenPcm
DopThenNativeThenPcm
NativeOnly
DopOnly
PcmOnly
```

### DoP

```text
Allowed
Forbidden
Preferred
```

## 5. ConversionPlan invariants

Обязательное правило:

```text
SignalPath::BitPerfectPcm
    ↔ ConversionPlan::None
```

Также:

```text
SignalPath::NativeDsd
    ↔ no PCM conversion
```

```text
SignalPath::DoP
    ↔ no DSD→PCM conversion
```

DSD→PCM:

```rust
ConversionPlan::DsdToPcm(...)
```

является DSP path и не может заявляться как Bit-Perfect.

## 6. UI

Рекомендуемая структура:

```text
Playback
────────────────────────
Policy:
  [ Best Possible ▼ ]

[ Дополнительно... ]
```

Внутри:

```text
Доступ к устройству
  ○ Автоматически
  ○ Shared / PipeWire
  ○ Exclusive / ALSA

PCM
  ☑ Разрешить SRC
  ☑ Разрешить изменение разрядности
  ☑ Разрешить изменение каналов

Частота
  ○ Исходная, если возможно
  ○ Наилучшая поддерживаемая
  ○ Максимальная

DSD
  [ Native → DoP → PCM ]

DoP
  ☑ Разрешить DoP
```

UI может отключать или объяснять параметры, которые не имеют смысла для текущей Policy. Но даже при ошибочном UI-состоянии Planner обязан отфильтровать недопустимый путь.

## 7. Settings

`src/settings.rs` отвечает за:

- загрузку;
- миграцию;
- `PlaybackPolicy`;
- `AdvancedPreferences`.

Но не выбирает фактический SignalPath.

Цепочка:

```text
settings.toml
      ↓
Settings
      ↓
PlaybackPolicy + AdvancedPreferences
      ↓
PathPlanner
```

## 8. Planner API

Целевая сигнатура:

```rust
pub fn plan(
    source: &SourceFormat,
    device: &DeviceCapabilities,
    policy: PlaybackPolicy,
    preferences: &AdvancedPreferences,
) -> Result<PathPlan, TrackRejection>;
```

Planner:

- не открывает device;
- не создаёт stream;
- не выполняет DSP;
- не взаимодействует с UI;
- не принимает RT decisions.

## 9. Примеры

### Strict + SRC enabled

```text
Policy = Strict
allow_resampling = true
```

SRC всё равно запрещён policy.

Если путь без SRC невозможен:

```text
TrackRejection::RequiresConversion
```

### BestPossible + SRC disabled

```text
Source = 24/192
DAC = max 96 kHz
allow_resampling = false
```

Результат — rejection, потому что допустимый путь требует SRC.

### BestPossible + Native → DoP → PCM

```text
Native = unavailable
DoP = available
PCM = available
```

Результат:

```text
SignalPath::DoP(...)
```

### Shared + PreferExclusive

```text
Policy = Shared
AccessPreference = PreferExclusive
```

Результат:

```text
BackendEndpoint::Shared(...)
```

## 10. Тестирование

Planner tests должны покрывать:

```text
Policy × AdvancedPreferences × DeviceCapabilities × SourceFormat
```

Минимум:

- Strict + SRC allowed;
- Strict + DSD→PCM allowed;
- Strict + DoP forbidden;
- Shared + PreferExclusive;
- BestPossible + SRC forbidden;
- BestPossible + Native unavailable;
- BestPossible + DoP forbidden;
- BestPossible + PCM fallback;
- несколько допустимых путей с разными preferences.

Важно проверить, что AdvancedPreferences не меняют hard invariants.

## 11. Acceptance Criteria

Реализация соответствует спецификации, если:

- «Дополнительно» сохраняется;
- пользователь может тонко ограничивать/предпочитать SignalPaths;
- AdvancedPreferences не являются вторым источником истины;
- Policy имеет абсолютный приоритет;
- Shared никогда не открывает raw `hw:*`;
- Strict никогда не получает DSP conversion;
- DSD→PCM никогда не помечается как Bit-Perfect;
- Planner остаётся единственной точкой выбора PathPlan;
- runtime исполняет уже выбранный PathPlan;
- UI показывает фактический путь, а не только выбранные preferences.

## 12. Итоговая схема

```text
                         SourceFormat
                              │
                     DeviceCapabilities
                              │
              ┌───────────────┴───────────────┐
              │                               │
      PlaybackPolicy                  AdvancedPreferences
       HARD RULES                    USER PREFERENCES
              │                               │
              └───────────────┬───────────────┘
                              ▼
                         PathPlanner
                              │
                              ▼
                           PathPlan
                           /      \
                          /        \
                    SignalPath   BackendEndpoint
                          \        /
                           \      /
                          Audio Runtime
```

Это дополнение расширяет v5.0 и не заменяет её базовую архитектуру.
