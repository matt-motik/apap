# _STATE_ — рабочее состояние сессии

> Точка входа для любой новой сессии (другая машина, другой агент, обрыв токенов/dead-loop).
> Обновляется в конце каждого шага и коммитится вместе с изменениями кода.

## Статус

- **Состояние:** `in_progress`.
- **Задача:** выполнение ROADMAP 4.4–4.7 (семантика настроек, сикбар, tray-volume, порядок на диске) после закрытой 4.1 (event-feed + дельта-синк).

## Активная задача

- **ROADMAP 4.4:** семантика настроек — diff-apply в `on_settings_save`, убрать eager-save, единый save-at-exit (окно + tray Quit). 4.1 закрыта (коммит текущего шага).
- Далее: 4.4 → 4.5 → 4.6 → 4.7 → финальные правки _STATE_/ROADMAP.

## Шаги

- [x] 1.1 player.rs: `core.lock().unwrap()` → graceful
- [x] 1.2 cover.rs: убрать агрессивную очистку очереди; playlist.rs: graceful при разрыве канала
- [x] 1.3 output.rs: неблокирующий probe вместо sleep
- [x] 2.2 settings.rs: убрать глобальный CONFIG (удалён целиком — был мёртвым code, DI уже явный)
- [x] 2.3 output.rs: trait AudioHost (+MockHost, choose_output, 9 тестов)
- [x] 3.2 cover.rs: hash-субдиректории кэша (O(1), legacy-fallback)
- [x] 3.3 mod.rs: асинхронная загрузка плейлиста при старте (drain_startup_tracks)
- [x] 4.2 decoder.rs: ISP — default-методы `seek`/`duration_secs`; дубли убраны; +тест на defaults
- [x] 4.3 тесты: player.rs (13 тестов), output.rs (9), MockSource/MockHost
- [x] 4.1 `src/app/events.rs` + дельта-синк: направленный event-feed (enum AppEvent + mpsc), эмиты в play_track/toggle/stop/auto-advance/scan/remove/clear/sort/load/volume/device/drain_cover; `drain_events()` в tick пушит tray-состояние сразу; `last_ui: UiState` — тик пишет только изменившееся. Тесты 49 lib + 6 bin, clippy без новых. (в этом коммите)
- [ ] 4.4 Семантика настроек: diff-apply + save-at-exit
- [ ] 4.5 Сикбар: TouchArea, seekbar_dragging/pending_seek, seek-commit при отпускании
- [ ] 4.6 Трей-громкость → UI-ползунок (событие VolumeChanged → UI-синк; убрать eager-save из wheel)
- [ ] 4.7 disk_tracks: порядок диска ≠ порядок просмотра, save_playlist только при выходе если dirty

## Следующий ход

1. **4.4:** прочитать `on_settings_save` (mod.rs), eager-save точки (mod.rs:313 shuffle, 337 volume, 864 wheel; ui_manager.rs:370 column widths; playlist_manager.rs:149 sort) и `SettingsStore::apply`/save.
2. Сделать diff-apply (только изменившиеся theme/device/columns/cover-size) в `on_settings_save`.
3. Убрать eager-`settings.save()` из хендлеров; ввести `queue_dirty`/saved-at-exit (окно close + tray Quit).
4. cargo build + clippy + test, коммит, обновить _STATE_.

## Изменяемые файлы (текущий шаг)

- `src/app/mod.rs` (on_settings_save, shuffle/volume хендлеры, quit paths)
- `src/app/playback_manager.rs` (volume wheel… 4.6, set_output_device уже эмитит)
- `src/app/playlist_manager.rs` (sort eager-save; 4.7: disk_tracks)
- `src/app/ui_manager.rs` (save_column_widths_from_ui)
- `src/settings.rs` (SettingsStore.apply/diff)
- `ui/top_panel.slint` (4.5), `ui/app.slint` (4.5 seek-commit)

## Риск / стоп-условие

- Не потерять сохранение настроек: save-at-exit обязан покрывать закрытие окна (close) и tray Quit; иначе изменения не доживут.
- 4.7: `save_playlist` при выходе должен брать `disk_tracks`, а не view-порядок; явные remove/clear обязаны трогать оба массива.
- Если diff-apply тянет переделки SettingsStore API — остановиться, пересмотреть.