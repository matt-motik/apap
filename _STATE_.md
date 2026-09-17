# Текущая микро-сессия

- **Задача из ROADMAP:** A3.1 — Модель настроек: `ExclusiveMode`, `FallbackPolicy`, `ResamplerMode`, `ClockFamily`, `FallbackRatePolicy`; расширение `AudioResamplerCfg` / `AudioCfg`; round-trip тесты
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/settings.rs`
- **Критерий успеха (Definition of Done):** `cargo check` без ошибок, `cargo clippy` без новых предупреждений в файле, `cargo test settings` зелёный (включая 4 новых теста из §11.1 спеки)

## Итерационный трекер
[x] Шаг 1: Добавить 5 enum-типов (`ExclusiveMode`, `FallbackPolicy`, `ResamplerMode`, `ClockFamily`, `FallbackRatePolicy`) с `serde(snake_case)`, `#[default]`, и `index()/from_index()` для UI. Проверка: `cargo check` без ошибок.

[ ] Шаг 2: Расширить `AudioResamplerCfg` полями `mode`, `fixed_rate`, `prefer_family`, `fallback_rate` и кастомным `Default`. Проверка: `cargo check` без ошибок.

[ ] Шаг 3: Расширить `AudioCfg` полями `exclusive`, `fallback`, `filter_hardware_only`, `filter_stereo_only` и кастомным `Default`. Проверка: `cargo check` без ошибок.

[ ] Шаг 4: Добавить тесты §11.1: `audio_resampler_defaults`, `audio_defaults_new_fields`, `audio_toml_roundtrip_with_new_fields`, `audio_partial_config_preserves_existing`. Проверка: `cargo test settings` зелёный, `cargo clippy` без новых предупреждений.

- **Текущий шаг (current_step):** Шаг 2
- **Следующий ход:** Расширить `AudioResamplerCfg` полями `mode`, `fixed_rate`, `prefer_family`, `fallback_rate` + кастомный `Default`, затем `cargo check`
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress