# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-10.5.1 «Вкладка "Кэш" в UI: отображение текущих размеров RAM / Disk» (+ .1a/.1b/.1c)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/audio/fulltrack.rs`
  - `src/app/fulltrack_manager.rs`
  - `src/app/mod.rs`
  - `ui/settings.slint`
  - `src/app/ui_manager.rs`
- **Критерий успеха (Definition of Done):** `cargo check` чистый, `cargo test` (130 lib + 10 bin) зелёный, `cargo clippy` без новых предупреждений в изменённых файлах; диалог настроек показывает актуальные размеры RAM/Disk-кэша визуализации (байты→человекочитаемый формат) при открытии и при тике.

## Итерационный трекер
[x] Шаг 1: `fulltrack.rs`: статистика дискового кэша — `disk_cache_size()` + ядро `disk_cache_size_in(dir) -> u64` (сумма всех файлов viz-каталога, отсутствующий каталог → 0). Проверка: `cargo test disk_cache_size` зелёный (`f42c62c`)
[x] Шаг 2: `fulltrack.rs`: человекочитаемый формат `fmt_cache_bytes(u64) -> String` (B/KiB/MiB, 2 знака) + юнит-тест. Проверка: `cargo test fmt_cache_bytes` зелёный (`064d947`)
[ ] Шаг 3: RAM-кэш: тип значения `CLruCache<String, slint::Image>` → `(slint::Image, usize)` (фактические байты из `rgba.len()`), обновить `put`/`get` в `fulltrack_manager.rs` и объявление поля в `app/mod.rs`. Проверка: `cargo check` без ошибок
[ ] Шаг 4: `fulltrack_manager.rs`: `pub(super) fn cache_sizes(&self) -> (u64, u64)` — суммарные RAM (итерация по `fulltrack_cache`) и Disk (вызов `disk_cache_size()`). Проверка: `cargo check` без ошибок
[ ] Шаг 5: `settings.slint`: вкладка «Cache» (секция статистики) — in-properties `cache-ram-size`, `cache-disk-size` (string) + две строки RowLabel. Проверка: `cargo build` компилирует .slint без ошибок
[ ] Шаг 6: `ui_manager.rs` (метод `sync_cache_stats_to_ui`) + `mod.rs` (вызов в `on_open_settings` и в `tick()` при открытом диалоге). Проверка: `cargo check` + `cargo clippy` без новых предупреждений

- **Текущий шаг (current_step):** Шаг 3
- **Следующий ход:** Изменить тип значения `fulltrack_cache` на `(slint::Image, usize)` (байты RGBA из `rgba.len()`): поле в `app/mod.rs`, `put` в `FullEvt::Ready` и `get` в `apply_fulltrack` в `fulltrack_manager.rs`. Затем `cargo check`.
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress