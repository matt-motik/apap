# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-10.4.2 — Disk-лимит кэша `cache.max_size_mb` + LRU-вытеснение по mtime на диске
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/audio/visualizer.rs`
  - `src/audio/fulltrack.rs`
  - `src/app/fulltrack_manager.rs`
- **Критерий успеха (Definition of Done):** `cargo check`, `cargo test`, `cargo clippy` зелёные; поле `disk_max_size_mb` (default 512) в `[visualization]`, функция `evict_disk_cache` (сканирование `viz_cache_dir()`, сумма, удаление oldest по mtime парами png+json, несуществующий каталог — no-op), вызов после `save_png` в run_osc/run_spec; покрыто юнит-тестами.

## Итерационный трекер
[x] Шаг 1: Поле `disk_max_size_mb: u32` (default 512, `serde(default)`) в `VisualizerSettings` + проброс в `VisualizerConfig`. Проверка: `cargo check` без ошибок
[ ] Шаг 2: Функция `evict_disk_cache(max_size_mb) -> io::Result<(usize, usize)>` в `audio/fulltrack.rs`: сумма пар png+json, `NonZero`-безопасный подсчёт, удаление oldest по mtime. Проверка: `cargo check` без ошибок
[ ] Шаг 3: Вызов `ft::evict_disk_cache(cfg.disk_max_size_mb)` после `save_png` в `run_osc` и `run_spec` (src/app/fulltrack_manager.rs). Проверка: `cargo check` без ошибок
[ ] Шаг 4: Юнит-тесты `evict_disk_cache` (перерасход → удаление старейших пар; в пределах лимита → без изменений; отсутствующий/пустой каталог → no-op). Проверка: `cargo test evict_disk_cache` зелёный
[ ] Шаг 5: Юнит-тесты roundtrip `disk_max_size_mb` (default 512, TOML, fallback) в `audio/visualizer.rs`. Проверка: `cargo test disk_disk_roundtrip` зелёный
[ ] Шаг 6: Финальная верификация `cargo test` + `cargo clippy` без новых warning → ROADMAP статус ✅ + консервация `_STATE_.md`. Проверка: полный зелёный прогон

- **Текущий шаг (current_step):** Шаг 2
- **Следующий ход:** Реализовать `evict_disk_cache(max_size_mb) -> io::Result<(usize, usize)>` в src/audio/fulltrack.rs, затем `cargo check`
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress