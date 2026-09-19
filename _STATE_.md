# Текущая микро-сессия

- **Задача из ROADMAP:** V5.1-B5 — освобождение exclusive `hw:`-устройства при простое
- **Вайтлист файлов в работе (Изменяемые файлы):**
  - `src/audio/player.rs`
  - `src/app/playback_manager.rs`
- **Критерий успеха (Definition of Done):** после `stop()` и при естественном конце плейлиста cpal-Stream (ALSA-нода `hw:*`) закрывается; `play()`/`toggle()` переоткрывают трек из `current_path`; `cargo test` и `cargo clippy` без новых предупреждений в изменённых файлах.

## Итерационный трекер
[x] Шаг 1: В `src/audio/player.rs` добавить поле `current_path: Option<PathBuf>` (заполняется в `open_pcm`/`open_dop`), публичный `release_engine()` (drop Stream+Worker, сохранить `shared`), внутренний `release_if_exclusive()` (только когда `stream_desc.exclusive`); `stop()` вызывает `release_if_exclusive()`. Проверка: `cargo check` без ошибок.
[x] Шаг 2: В `src/audio/player.rs` `play()`/`toggle()`: если `self.stream.is_none()` → переоткрыть текущий трек через `open(current_path)` перед управлением флагами. Проверка: `cargo check` без ошибок.
[x] Шаг 3: В `src/app/playback_manager.rs` `handle_auto_advance` (ветка `None`, конец плейлиста) вызывать `player.release_if_exclusive()` после `clear_end()`. Проверка: `cargo check` без ошибок.
[x] Шаг 4: Юнит-тесты в `player.rs`: (а) `stop()` на exclusive-описе дропает stream+worker, сохраняет `shared`; (б) `play()` после release переоткрывает движок (stream снова Some). Проверка: `cargo test player::` зелёный.

- **Текущий шаг (current_step):** Шаг 4 — закоммитить; проверить финальную верификацию
- **Следующий ход:** git-коммит изменений player.rs + playback_manager.rs + _STATE_.md; затем Шаг 5 (закрытие сессии)
- **Счетчик безуспешных компиляций:** 0/3
- **Состояние:** in_progress