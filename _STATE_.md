# _STATE_ — рабочее состояние сессии

> Точка входа для любой новой сессии (другая машина, другой агент, обрыв токенов/dead-loop).
> Обновляется в конце каждого шага и коммитится вместе с изменениями кода.

## Статус

- **Состояние:** `in_progress`.
- **Задача:** выполнение ROADMAP по приоритету, минимальными шагами.

## Активная задача

- **ROADMAP 4.1:** mod.rs — шина событий (event-driven связь компонентов UI ↔ audio). 4.2 (ISP для AudioSource) закрыта.
- Далее по плану: 4.1 (event bus) → 5.x.

## Шаги

- [x] 1.1 player.rs: `core.lock().unwrap()` → graceful
- [x] 1.2 cover.rs: убрать агрессивную очистку очереди; playlist.rs: graceful при разрыве канала
- [x] 1.3 output.rs: неблокирующий probe вместо sleep
- [x] 2.2 settings.rs: убрать глобальный CONFIG (удалён целиком — был мёртвым code, DI уже явный)
- [x] 2.3 output.rs: trait AudioHost (+MockHost, choose_output, 9 тестов)
- [x] 3.2 cover.rs: hash-субдиректории кэша (O(1), legacy-fallback)
- [x] 3.3 mod.rs: асинхронная загрузка плейлиста при старте (drain_startup_tracks)
- [x] 4.3 тесты: player.rs (13 тестов: переходы, volume/mute, EOF, lock-конфликт, snapshot) — mock AudioSource, test_new() без cpal
- [x] 4.2 decoder.rs: ISP — default-методы `seek` (Err «not supported») и `duration_secs` (из `info().num_frames`); дублирующие impl убраны из Decoder/DsdDecoder/MockSource; +тест на defaults
- [ ] 4.1 mod.rs: шина событий

## Следующий ход

1. Прочитать mod.rs (MusicApp, tick 100ms), playback_manager, ROADMAP §4.1.
2. Определить форму шины: enum событий (TrackChanged, PlaybackStateChanged, QueueChanged, CoverChanged, SettingsChanged), подписчики UI.
3. Решить минимальный объём: полная шина или выборочно убрать прямой проброс колбэков между менеджерами.
4. cargo build + clippy + test, коммит, обновить _STATE_.

## Изменяемые файлы (текущий шаг)

- `src/app/mod.rs`, `src/app/playback_manager.rs`, `src/app/playlist_manager.rs`, `src/app/ui_manager.rs` (4.1)
- (4.2 закрыта: `src/audio/decoder.rs`, `src/audio/dsd.rs`, `src/audio/player.rs` — trait AudioSource + defaults)

## Риск / стоп-условие

- Не менять контракты методов Player без нужды; сохранить обратную совместимость с UI/tray.
- Если рефакторинг одного пункта тянет переделки соседних — остановиться, пересмотреть.