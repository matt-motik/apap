# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-7.5 — Значок-индикатор «Bit-perfect» в статус-баре, [ТЗ §7.5](docs/spec_visualizer_v5.1.md#75-индикация)
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `ui/status.slint`
  - `ui/app.slint`
- **Критерий успеха (Definition of Done):** в статус-баре отображается значок-индикатор «Bit-perfect», когда активен `settings.audio.bit_perfect`; используется существующий корневой проп `bit-perfect` (пишется в `sync_playback_state_to_ui`), новых свойств не требуется; `cargo check`, `cargo test`, `cargo clippy` зелёные без новых предупреждений.

## Итерационный трекер

[x] Шаг 1: ui/status.slint + ui/app.slint — проп `bp-active` в StatusBar + бейдж «Bit-perfect» (цвет text-dim/accent), binding `bp-active: root.bit-perfect` в AppWindow. Проверка: `cargo check` без ошибок.

[ ] Шаг 2: Верификация (`cargo test`, `cargo clippy` 0 новых), ROADMAP V5.1-7.5 → ✅ + _STATE_.md → done, финальный коммит. Проверка: тесты зелёные, clippy чист.

- **Текущий шаг (current_step):** Шаг 2
- **Следующий ход:** Полная верификация (cargo test, clippy), ROADMAP V5.1-7.5 → ✅, закрыть _STATE_.md, финальный коммит
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress