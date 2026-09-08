# _STATE_ — рабочее состояние сессии

> Точка входа для любой новой сессии (другая машина, другой агент, обрыв токенов/dead-loop).
> Обновляется в конце каждого шага и коммитится вместе с изменениями кода.

## Статус

- **Состояние:** `in_progress`.
- **Задача:** выполнение ROADMAP 4.4–4.7 (семантика настроек, сикбар, tray-volume, порядок на диске) после закрытых 4.1, 4.5, 4.6.

## Активная задача

- **ROADMAP 4.4:** семантика настроек — diff-apply в `on_settings_save` (только изменившиеся theme/device/columns/cover-size), убрать eager-`settings.save()` из хендлеров (shuffle, volume GUI+wheel), единый save-at-exit (окно + tray Quit). 4.7 закрыта (disk_tracks + save только при выходе если dirty).

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
- [x] 4.1 event-feed + дельта-синк (`src/app/events.rs`)
- [x] 4.5 сикбар: TouchArea(grab) поверх Slider, seek-commit при отпускании; тик не трогает сикбар/pos/dur при драге
- [x] 4.6 Трей-громкость → UI-ползунок (пассивно через дельта-синк тика)
- [x] 4.7 disk_tracks: порядок диска ≠ порядок просмотра; save_playlist пишет disk_tracks; eager-save убраны (scan/remove/clear/sort); сохранение только при выходе если playlist_dirty (трей Quit + close окна)
- [ ] 4.4 Семантика настроек: diff-apply + save-at-exit

## Следующий ход

1. **4.4:** прочитать `on_settings_save` (mod.rs), eager-save точки (mod.rs: shuffle, volume GUI+wheel; ui_manager.rs: column widths; playlist_manager.rs: sort preferences) и `SettingsStore`.
2. Сделать diff-apply (только изменившиеся theme/device/columns/cover-size) в `on_settings_save`.
3. Убрать eager-`settings.save()` из хендлеров; единый save-at-exit (окно close + tray Quit), сохраняя явный Save в Settings-диалоге.
4. cargo build + clippy + test, коммит, обновить _STATE_.

## Изменяемые файлы (текущий шаг)

- `src/app/mod.rs` (on_settings_save, shuffle/volume/close-quit хендлеры)
- `src/app/playback_manager.rs` (volume wheel eager-save)
- `src/app/playlist_manager.rs` (sort preferences eager-save)
- `src/app/ui_manager.rs` (save_column_widths_from_ui)
- `src/settings.rs` (SettingsStore.apply/diff)

## Риск / стоп-условие

- Не потерять сохранение настроек: save-at-exit обязан покрывать оба выхода (window close без minimize + tray Quit); явный Save-кнопок (Settings, Save playlist) не должен пострадать.
- diff-apply не должен размазать изменение device: apply плавно, не сбрасывая другие поля.
- Если diff-apply тянет переделки SettingsStore API — остановиться, пересмотреть.