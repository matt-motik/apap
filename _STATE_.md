# _STATE_ — рабочее состояние сессии

> Точка входа для любой новой сессии (другая машина, другой агент, обрыв токенов/dead-loop).
> Обновляется в конце каждого шага и коммитится вместе с изменениями кода.

## Статус

- **Состояние:** `in_progress`.
- **Задача:** выполнение ROADMAP 4.4–4.7 (семантика настроек, сикбар, tray-volume, порядок на диске) после закрытых 4.1, 4.5, 4.6.

## Активная задача

- **ROADMAP 4.7:** `playlist_manager.rs` — ввести `disk_tracks: Vec<Track>` (порядок загрузки + скана). `save_playlist()` пишет `disk_tracks` (не view-порядок). `save_playlist()` вызывать только при выходе и только если `queue_dirty`. Ассеты: remove/all, clear, drain_scan, sort, exit-пути. 4.6 закрыта (пассивно: дельта-синк обновляет UI-ползунок через 100 мс тик).
- Далее: 4.7 → 4.4 → финальные правки _STATE_/ROADMAP.

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
- [ ] 4.7 disk_tracks: порядок диска ≠ порядок просмотра
- [ ] 4.4 Семантика настроек: diff-apply + save-at-exit

## Следующий ход

1. **4.7:** прочитать `playlist_manager.rs` (drain_scan, load_playlist, sort_tracks, remove_track, clear_playlist, save_playlist) и `mod.rs` (exit paths: TrayCmd::Quit, on-close).
2. Ввести поле `disk_tracks: Vec<Track>` и заполнять при `drain_startup_tracks`, `load_playlist`, `drain_scan` (append к disk_tracks, и к tracks).
3. `save_playlist()` → брать из `disk_tracks`, не `self.tracks`. Убрать eager-вызовы `save_playlist()` из drain_scan/remove/clear/sort.
4. Флаг `queue_dirty`: выставлять при добавлении/удалении треков (drain_scan по завершении, remove_track, clear_playlist), НЕ при sort/load/startup.
5. Exit-пути: в TrayCmd::Quit и on-close-колбэке вызывать `save_playlist()` если `queue_dirty`.
6. cargo build + clippy + test, коммит, обновить _STATE_.

## Изменяемые файлы (текущий шаг)

- `src/app/playlist_manager.rs` (disk_tracks, queue_dirty, save_playlist)
- `src/app/mod.rs` (exit paths, drain_startup_tracks/load_playlist заполнение disk_tracks)
- далее 4.4: `src/app/ui_manager.rs`, `src/settings.rs`

## Риск / стоп-условие

- Не потерять сохранение настроек: save-at-exit обязан покрывать закрытие окна (close) и tray Quit; иначе изменения не доживут.
- 4.7: `save_playlist` при выходе должен брать `disk_tracks`, а не view-порядок; явные remove/clear обязаны трогать оба массива.
- Если diff-apply тянет переделки SettingsStore API — остановиться, пересмотреть.