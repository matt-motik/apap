# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-7.4 — Предупреждение «DSD→PCM несовместимо с bit-perfect для DSD» + логика переключения сценариев, [ТЗ §7.4](docs/spec_visualizer_v5.1.md#74-dsd-в-bit-perfect), [ТЗ §7.5](docs/spec_visualizer_v5.1.md#75-индикация), [ТЗ §8.4](docs/spec_visualizer_v5.1.md#84-взаимодействие-с-bit-perfect)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/settings.rs`
  - `src/app/ui_manager.rs`
  - `src/app/mod.rs`
  - `ui/status.slint`
  - `ui/settings.slint`
  - `ui/app.slint`
- **Критерий успеха (Definition of Done):** детекция сценария `bit_perfect ∧ DSD ∧ dsd.mode = pcm` (конфликт); предупреждение «DSD→PCM несовместимо с bit-perfect для DSD» в диалоге настроек (Audio) при конфликте; выбор DSD-режима (PCM/Native/DoP) в настройках применяется к плееру и перезапускает текущий DSD-трек; индикатор «Не bit-perfect (DSD→PCM)» в статус-баре при активном конфликте на DSD-треке; `cargo check`, `cargo test`, `cargo clippy` зелёные без новых предупреждений.

## Итерационный трекер

[x] Шаг 1: settings.rs — `DsdMode::index()/from_index(usize)` + `Settings::dsd_pcm_breaks_bit_perfect()` (bit_perfect && mode==Pcm) + юнит-тест. Проверка: `cargo check` + `cargo test dsd_pcm` зелёный.

[x] Шаг 2: ui/status.slint + ui/app.slint — индикатор статус-бара «Не bit-perfect (DSD→PCM)» (`dsd-not-bp`, цвет text-error), AppWindow-проп `status-dsd-not-bp` + binding. Проверка: `cargo check` без ошибок.

[x] Шаг 3: ui/settings.slint + ui/app.slint — в Audio-вкладке: ComboBox «DSD mode» (PCM/Native/DoP) + warning «DSD→PCM несовместимо с bit-perfect для DSD» `visible: dsd-bp-warn && dsd-mode == 0`, пропсы `dsd-mode`/`dsd-bp-warn`, callback `set-dsd-mode`. Проверка: `cargo check` без ошибок.

[x] Шаг 4: src/app/ui_manager.rs + src/app/mod.rs — `sync_dsd_settings_to_ui()` (dsd-mode/warn) в `sync_settings_to_ui`, `sync_dsd_status_ui()` (индикатор статус-бара по текущему DSD-треку и конфликту) с вызовом в `tick()`. Проверка: `cargo check` без ошибок.

[x] Шаг 5: src/app/mod.rs — callback `settings-set-dsd-mode` (правка draft, live-warn) + применение в `on_settings_save` (`player.set_dsd_mode`, перезапуск текущего DSD-трека `play_track`, sync dsd UI). Проверка: `cargo check` без ошибок.

[ ] Шаг 6: Верификация и коммит — `cargo test` + `cargo clippy` (0 новых предупреждений), ROADMAP V5.1-7.4 → ⏳/✅, git-коммит (код + `_STATE_.md`). Проверка: тесты зелёные, clippy чисто.

- **Текущий шаг (current_step):** Шаг 6
- **Следующий ход:** Полная верификация (cargo test bin+lib, cargo clippy), обновить ROADMAP V5.1-7.4 → ✅ и закрыть _STATE_.md, финальный коммит
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress