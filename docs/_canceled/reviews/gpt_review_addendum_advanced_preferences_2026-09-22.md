# Дополнение к Code Review APAP — Advanced Playback Preferences

**Дата:** 2026-09-22  
**Связано с:** `apap_code_review_new_policy_2026-09-21.md`  
**Архитектурный контекст:** `PlaybackPolicy → PathPlanner → SignalPath → BackendEndpoint`

## 1. Цель

Сохранить существующий пользовательский пункт **«Дополнительно»**, но перевести его из набора разрозненных технических флагов в единый слой пользовательских предпочтений.

> Advanced-настройки могут сужать или выбирать допустимые варианты пути, но не могут нарушать инварианты выбранной `PlaybackPolicy`.

## 2. Модель

```text
PlaybackPolicy
      │ hard constraints
      ▼
AdvancedPreferences
      │ preferences / constraints
      ▼
PathPlanner
      │
      ▼
PathPlan
```

Приоритет:

1. Инварианты `PlaybackPolicy`.
2. Возможности устройства.
3. Ограничения/предпочтения пользователя.
4. Выбор конкретного допустимого `SignalPath`.

## 3. Что дать в «Дополнительно»

Предлагаемый логический набор:

```text
AdvancedPreferences
├── access preference
├── PCM conversion preferences
├── sample-rate preference
├── DSD path preference
├── DoP preference
└── compatibility/fallback preferences
```

Это не означает, что все пункты обязаны быть сразу видимы в UI.

## 4. Доступ к устройству

Предпочтения:

```text
Automatic
Prefer Shared / PipeWire
Prefer Exclusive / ALSA
```

Это не команда `snd_pcm_open()`. Planner обязан проверить совместимость с Policy.

В частности:

- `Shared` не может превратиться в raw `hw:*`;
- `Strict` сохраняет требование exclusive;
- endpoint выбирается Planner.

## 5. PCM

Можно дать пользователю ограничения:

```text
Разрешить SRC
Разрешить изменение разрядности
Разрешить изменение числа каналов
```

И предпочтение частоты:

```text
Исходная, если возможно
Наилучшая поддерживаемая
Максимальная поддерживаемая
```

Это только входные данные Planner. Они не запускают DSP напрямую.

## 6. DSD

Полезен порядок:

```text
Native DSD
DoP
DSD → PCM
```

Пользователь может задать предпочтительный порядок или ограничить варианты.

Например `Native only` при отсутствии Native должен приводить к `TrackRejection`, а не к скрытому fallback.

## 7. DoP

DoP остаётся частью `SignalPath`:

```rust
SignalPath::DoP(DopFormat)
```

Advanced preference может разрешать, запрещать или предпочитать DoP. Формирование DoP выполняется runtime соответствующего пути.

## 8. DSD → PCM

```rust
ConversionPlan::DsdToPcm(...)
```

является DSP path и никогда не должен считаться Bit-Perfect.

Для `Strict` DSD→PCM всегда запрещён независимо от AdvancedPreferences.

## 9. Конфликтующие старые флаги

Модель вида:

```text
bit_perfect
exclusive
resampler
dsd.mode
fallback
```

не должна оставаться вторым источником истины.

Цель:

```text
PlaybackPolicy
+
AdvancedPreferences
```

## 10. Mapping на текущий проект

### `src/settings.rs`

Хранить `PlaybackPolicy` и `AdvancedPreferences`; выполнить миграцию старых настроек.

### `src/audio/policy.rs`

Hard constraints политики.

### `src/audio/planner.rs`

Получает:

```text
SourceFormat
DeviceCapabilities
PlaybackPolicy
AdvancedPreferences
```

и является единственной точкой выбора `PathPlan`.

### `src/audio/signal_path.rs`

Фактический SignalPath.

### `src/audio/endpoint.rs`

Shared/Exclusive endpoints.

### `src/audio/output.rs`

Исполнение выбранного endpoint/path; не принимает policy decisions.

### `src/app/playback_manager.rs`

Не интерпретирует AdvancedPreferences; обрабатывает `PathPlan`, `TrackRejection` и fatal errors раздельно.

## 11. Тесты

Минимум:

1. Strict + SRC allowed → SRC всё равно запрещён.
2. Strict + DSD→PCM allowed → DSD→PCM запрещён.
3. Shared + PreferExclusive → Shared endpoint.
4. BestPossible + SRC forbidden + DAC max96 + source192 → rejection.
5. BestPossible + DoP allowed → DoP может быть выбран.
6. BestPossible + DoP forbidden + Native unavailable + PCM allowed → DSD→PCM.
7. Strict + Native/DoP unavailable → rejection.
8. Несколько допустимых путей → проверка user preference порядка.

## 12. Acceptance Criteria

- «Дополнительно» сохраняется.
- AdvancedPreferences не создают вторую audio architecture.
- Policy имеет приоритет.
- Planner — единственная точка выбора SignalPath.
- AdvancedPreferences не открывают устройства.
- Strict нельзя превратить в DSP-путь через UI.
- Shared нельзя превратить в raw `hw:*` через UI.
- Фактический SignalPath отображается независимо от preferences.
- Причина `TrackRejection` остаётся диагностируемой.

## 13. Итог

> **«Дополнительно» — это набор пользовательских предпочтений и ограничений для PathPlanner, а не набор низкоуровневых переключателей аудиодвижка.**

Так сохраняется тонкая настройка для опытного пользователя без разрушения гарантий новой архитектуры.
