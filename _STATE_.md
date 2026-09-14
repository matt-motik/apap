# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-10.4.3 — TTL-инвалидация по mtime + расширение ключа кэша (mtime+size+mode+channels+params+dsd)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - src/audio/visualizer.rs
  - src/audio/fulltrack.rs
  - src/app/fulltrack_manager.rs
- **Критерий успеха (Definition of Done):** cargo check/clippy/test зелёные; ключи включают dsd_params при DSD-треке; TTL-проверка использует sidecar mtime с суб-секундной точностью.

## Итерационный трекер

[x] Шаг 1: Добавить `DsdCacheParams` в `visualizer.rs` (новая структура + поле в `VisualizerConfig` + `from_settings`). Проверка: `cargo check`.
[x] Шаг 2: Расширить `cache_key`/`cache_key_spectrogram` параметром `dsd: Option<&DsdCacheParams>`, хешировать DSD-поля при Some. Обновить 4 сайта вызова в `fulltrack_manager.rs` (run_osc, run_spec, check_settings x2) + существующий тест. Проверка: `cargo check` + `cargo test cache_key`.
[x] Шаг 3: Улучшить `cache_meta_valid` — сравнивать mtime источника с mtime sidecar-файла (суб-секундная точность через SystemTime). Добавить `cache_meta_valid_in(dir, key)` для тестопригодности. Проверка: `cargo check` + `cargo test`.
[x] Шаг 4: Тесты — DSD-ключ меняется при смене DSD-параметров; TTL: source mtime > sidecar mtime → невалидно; PNG отсутствует → невалидно. Проверка: `cargo test`.
[ ] Шаг 5: Финальная верификация (cargo check + cargo clippy + cargo test), обновление ROADMAP.md (✅), консервация _STATE_.md.

- **Текущий шаг (current_step):** Шаг 5
- **Следующий ход:** Прогнать полный `cargo test` и `cargo clippy`, обновить статусы в ROADMAP.md (V5.1-10.4.3, .3a, .3b → ✅ с хешем коммита), законсервировать `_STATE_.md`.
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress
