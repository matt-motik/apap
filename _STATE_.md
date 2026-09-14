# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-10.4.1 — RAM-лимит кэша визуализации `viz_max_ram_mb` (=64 MB) → параметризация `CLruCache`
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/audio/visualizer.rs`
  - `src/audio/fulltrack.rs`
  - `src/app/mod.rs`
- **Критерий успеха (Definition of Done):** `cargo check`, `cargo test`, `cargo clippy` зелёные; `CLruCache` создаётся из `settings.visualization.viz_max_ram_mb` (TOML `[visualization]`), при изменении настройки через Save происходит горячий `resize`; math + roundtrip покрыты юнит-тестами.

## Итерационный трекер
[x] Шаг 1: Поле `viz_max_ram_mb: u32` (default 64, `serde(default)`) в `VisualizerSettings` + проброс в `VisualizerConfig`. Проверка: `cargo check` без ошибок
[x] Шаг 2: Чистая функция `fulltrack_cache_max_entries(max_ram_mb) -> NonZeroUsize` в `audio/fulltrack.rs` (формула `mb / avg_rgba_mb`, avg = 2000×512×4 B). Проверка: `cargo check` без ошибок
[x] Шаг 3: Убрать константу `FULLTRACK_CACHE_LEN`, инициализация `CLruCache` от `ft::fulltrack_cache_max_entries(viz_max_ram_mb)` в `MusicApp::new`. Проверка: `cargo check` без ошибок
[ ] Шаг 4: Горячий `CLruCache::resize` в `settings_save` при изменении `viz_max_ram_mb`. Проверка: `cargo check` без ошибок
[ ] Шаг 5: Юнит-тесты: math `fulltrack_cache_max_entries` (64→16, минимум 1) в `audio/fulltrack.rs` + toml-roundtrip `viz_max_ram_mb` в `audio/visualizer.rs`. Проверка: `cargo test` зелёный
[ ] Шаг 6: Финальная верификация `cargo test` + `cargo clippy` без новых warning → ROADMAP статус ✅ + консервация `_STATE_.md`. Проверка: полный зелёный прогон

- **Текущий шаг (current_step):** Шаг 4
- **Следующий ход:** В `settings_save` (src/app/mod.rs) зафиксировать старое `viz_max_ram_mb` до применения draft, после Save при изменении — `self.fulltrack_cache.resize(...)`, затем `cargo check`
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress