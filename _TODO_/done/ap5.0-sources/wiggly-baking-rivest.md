# Правки в APAP_architecture_spec_v5.0: слой PlaybackPreferences

## Контекст

В предыдущем обсуждении пользователь предложил добавить в архитектуру v5.0
(`PlaybackPolicy → PathPlanner → SignalPath → BackendEndpoint`) промежуточный
слой пользовательских предпочтений (`PlaybackPreferences` /
`AdvancedConstraints`), который соответствует разделу UI «Дополнительно» из
старого интерфейса. Идея — сохранить «Дополнительно» не как четвёртую
политику, а как сужающий фильтр внутри уже выбранной `PlaybackPolicy`,
не способный нарушить её инварианты (например, `Strict + SRC` остаётся
невозможным состоянием).

Я как ревьюер согласился с идеей в целом и указал на три конкретные правки,
пользователь подтвердил согласие со всеми тремя:

1. Пункт «Доступ к устройству» в блоке «Дополнительно» — оставить только как
   **информацию** (read-only отображение фактически выбранного
   `BackendAccess`), без возможности изменения пользователем. Он и так
   полностью выводится из `PlaybackPolicy` по правилу §3 спеки
   (`Shared→Shared`, `BestPossible/Strict→Exclusive`) — предоставлять по
   нему отдельный переключатель в Advanced было бы архитектурной дырой,
   повторяющей проблему «Strict + SRC» на другом поле.
2. Не плодить независимые булевы свитчи внутри `PlaybackPreferences`
   (`allow_dsd_to_pcm: bool` рядом с `preferred_dsd_path`) — объединить в
   один enum `DsdPathPreference` с вариантами `NativeOnly` /
   `NativeThenDop` / `NativeThenDopThenPcm`, чтобы противоречивая
   комбинация была невыразима на уровне типов.
3. Различать в `TrackRejection` (или в обёртке над ним) причину отказа:
   `RejectedByPolicy` (политика в принципе не допускает такой путь) vs
   `RejectedByPreference` (путь допустим политикой, но отсечён
   пользовательским Advanced-предпочтением) — для корректной диагностики в
   UI («Показывать причину преобразования»), чтобы пользователь чинил
   правильную настройку.

Задача этой сессии — внести согласованные правки в сам файл спецификации
`APAP_architecture_spec_v5.0_PlaybackPolicy_PathPlanner_SignalPath_BackendEndpoint.md`
(в `C:\Users\HP\Downloads\`), чтобы решение было зафиксировано письменно, а
не осталось только в чате.

## Изменения в файле спецификации

Файл: `C:\Users\HP\Downloads\APAP_architecture_spec_v5.0_PlaybackPolicy_PathPlanner_SignalPath_BackendEndpoint.md`

### 1. Новый раздел «2a. PlaybackPreferences» — после раздела «2. PlaybackPolicy» (после строки 55, `pub enum PlaybackPolicy {...}`), перед разделом «3. BackendAccess и BackendEndpoint» (строка 92)

Добавить текст (по-русски, в стиле остального документа), включающий:

- Формулировку принципа: `PlaybackPreferences` — это слой пользовательских
  предпочтений, который **только сужает** множество допустимых `SignalPath`,
  вычисленное `PlaybackPolicy`, и никогда не может его расширить. Пустое
  пересечение → `TrackRejection`.
- Rust-структуру:
  ```rust
  pub struct PlaybackPreferences {
      pub preferred_dsd_path: DsdPathPreference,
      pub preferred_sample_rate: SampleRatePreference,
      pub pcm_allow_rate_change: bool,
      pub pcm_allow_bitdepth_change: bool,
      pub pcm_allow_channel_change: bool,
      pub prefer_pipewire_for_shared: bool,
      pub allow_fallback: bool,
  }

  pub enum DsdPathPreference {
      NativeOnly,
      NativeThenDop,
      NativeThenDopThenPcm,
  }

  pub enum SampleRatePreference {
      BestSupported,
      Maximum,
  }
  ```
  (без отдельного `allow_dsd_to_pcm: bool` — он поглощён в
  `DsdPathPreference::NativeThenDopThenPcm`).
- Явно указать: **`BackendAccess` не входит в `PlaybackPreferences`** — он
  всегда выводится из `PlaybackPolicy` по таблице в §3 и в UI «Дополнительно»
  показывается только как информация (`Доступ к устройству: <вычислено>`),
  без контрола выбора.
- Диаграмму (текстовую, как остальные в файле):
  ```text
  PlaybackPolicy
         │
         ▼
  PlaybackPreferences  (только сужение, не может нарушить policy)
         │
         ▼
     PathPlanner
         │
         ▼
      PathPlan
  ```
- Пример (как в исходном сообщении пользователя): `Best Possible` +
  `DsdPathPreference::NativeThenDop` → при недоступном Native DSD получаем
  `SignalPath::DoP`; при `DsdPathPreference::NativeOnly` в той же ситуации —
  `TrackRejection::NativeDsdUnavailable { source: RejectionSource::Preference }`.

### 2. Правка сигнатуры `PathPlanner::plan()` — раздел «7. PathPlanner» (строки 214-230)

Заменить текущую сигнатуру:
```rust
pub fn plan(
    source: &SourceFormat,
    device: &DeviceCapabilities,
    policy: PlaybackPolicy,
) -> Result<PathPlan, TrackRejection>;
```
на:
```rust
pub fn plan(
    source: &SourceFormat,
    device: &DeviceCapabilities,
    policy: PlaybackPolicy,
    preferences: &PlaybackPreferences,
) -> Result<PathPlan, TrackRejection>;
```
и добавить абзац: «Planner сначала вычисляет допустимое политикой множество
`SignalPath` (см. §2/§14), затем пересекает его с `PlaybackPreferences`.
Preferences могут только сужать это множество; если пересечение пустое —
`TrackRejection` с полем-источником (`RejectionSource::Policy` /
`RejectionSource::Preference`), см. §9».

### 3. Правка раздела «9. TrackRejection» (строки 298-326)

Добавить обёртку/поле, различающее источник отказа:
```rust
pub enum RejectionSource {
    /// Путь запрещён самой политикой (например, Strict + SRC).
    Policy,
    /// Путь допустим политикой, но отсечён пользовательским
    /// Advanced-предпочтением (например, DsdPathPreference::NativeOnly).
    Preference,
}

pub struct TrackRejectionInfo {
    pub reason: TrackRejection,
    pub source: RejectionSource,
}
```
С пояснением: UI-диагностика («Показывать причину преобразования») обязана
показывать `source`, чтобы пользователь понимал, что чинить — политику или
свою собственную Advanced-настройку.

### 4. Правка раздела «13. Сопоставление с текущими файлами» → подраздел `src/settings.rs` (строки 587-601)

Добавить пункт: сохранить `playback_preferences: PlaybackPreferences` как
отдельное поле рядом с `playback_policy`, с той же миграцией старого TOML
(`bit_perfect`/`exclusive`/`fallback`/`resampler` → маппинг в
`preferred_*`/`allow_*` поля, где это осмысленно, остальное — дефолты).

### 5. Правка раздела «14. Запрещённые комбинации» (строки 652-667)

Добавить в список запрещённых состояний:
```text
PlaybackPreferences изменяет BackendAccess в обход PlaybackPolicy
PlaybackPreferences расширяет множество SignalPath за пределы,
  допустимые PlaybackPolicy (например, Strict + allow_dsd_to_pcm=true
  должно быть невозможно на уровне типа DsdPathPreference)
```

### 6. Правка раздела «17. Приоритеты» → добавить в P1 (после текущего пункта 5, строки 765-772)

Новый пункт: «6. `PlaybackPreferences` как отдельный тип + intersection-логика
в Planner (см. §2a); RejectionSource в TrackRejection (см. §9)».

### 7. Правка раздела «19. Acceptance criteria» (строки 801-822)

Добавить критерии:
```text
18. PlaybackPreferences не может расширить множество путей, допустимых
    PlaybackPolicy — покрыто property-тестом на всей матрице
    policy × preferences.
19. UI «Дополнительно» отображает «Доступ к устройству» как информацию,
    без возможности редактирования пользователем.
20. TrackRejection различает RejectionSource::Policy и
    RejectionSource::Preference.
```

## Проверка

Это правка markdown-документации, не кода — верификация означает:
- Файл остаётся валидным Markdown (согласованные заголовки/нумерация разделов
  не ломаются; новый «2a» не переопределяет существующую нумерацию 3-50,
  поэтому используется суффикс «a», а не сдвиг всех номеров).
- Перечитать итоговый файл целиком после правок и убедиться, что новый
  раздел логически согласован с §3 (BackendAccess), §7 (Planner), §9
  (TrackRejection), §14 (запрещённые комбинации) — противоречий нет.
- Показать пользователю итоговый диф/выдержку нового раздела в чате для
  финального подтверждения формулировок.
