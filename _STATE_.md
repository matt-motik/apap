# _STATE_ — рабочее состояние сессии

> Точка входа для любой новой сессии (другая машина, другой агент, обрыв токенов/dead-loop).
> Обновляется в конце каждого шага и коммитится вместе с изменениями кода.

## Статус

- **Состояние:** `in_progress`.
- **Задача:** выполнение ROADMAP 4.4–4.7 (семантика настроек, сикбар, tray-volume, порядок на диске) после закрытой 4.1 (event-feed + дельта-синк).

## Активная задача

- **ROADMAP 4.6:** tray-volume → UI-ползунок: событие `VolumeChanged` уже эмитится; обеспечить, чтобы дельта-синк обновлял слайдер громкости; убрать eager-save из wheel (доедет на 4.4 save-at-exit). 4.1 и 4.5 закрыты (коммиты текущей сессии).
- Далее: 4.6 → 4.7 → 4.4 → финальные правки _STATE_/ROADMAP.

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
- [x] 4.5 сикбар: TouchArea(grab) поверх Slider, seek-dragging/pending-seek, seek-commit при отпускании; тик не трогает сикбар/pos/dur при драге (top_panel.slint app.slint, on_seek_commit в mod.rs)
- [ ] 4.6 Трей-громкость → UI-ползунок (VolumeChanged уже в feed)
- [ ] 4.7 disk_tracks: порядок диска ≠ порядок просмотра
- [ ] 4.4 Семантика настроек: diff-apply + save-at-exit

## Следующий ход

1. **4.6:** проверить, долетает ли `VolumeChanged` до слайдера; в `sync_playback_state_to_ui` слайдер громкости пишется из `volume` (уже). При запуске окна `set_volume(v)` из settings.
2. Убрать eager-`settings.save()` из wheel трея (доедет на выходе, вместе с 4.4).
3. cargo build + clippy + test, коммит, обновить _STATE_.

## Изменяемые файлы (текущий шаг)

- `src/app/mod.rs` (wheel tray eager-save)
- `src/app/playback_manager.rs` (set_output_device/volume-эмиты)
- `src/app/ui_manager.rs` (sync громкости)
- далее 4.7: `src/app/playlist_manager.rs`, 4.4: settings

## Риск / стоп-условие

- Не потерять сохранение настроек: save-at-exit обязан покрывать закрытие окна (close) и tray Quit; иначе изменения не доживут.
- 4.7: `save_playlist` при выходе должен брать `disk_tracks`, а не view-порядок; явные remove/clear обязаны трогать оба массива.
- Если diff-apply тянет переделки SettingsStore API — остановиться, пересмотреть.