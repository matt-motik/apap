# _STATE_ — рабочее состояние сессии

> Точка входа для любой новой сессии (другая машина, другой агент, обрыв токенов/dead-loop).
> Обновляется в конце каждого шага и коммитится вместе с изменениями кода.

## Статус

- **Состояние:** `done` — цикл ROADMAP 4.4–4.7 завершён (все шаги [x], тесты зелёные).

## Активная задача

- Нет активной задачи. Следующий эпизод: future-блок ROADMAP (F1–F4) или новый план.

## Выполнено в этой сессии

- 4.1 `src/app/events.rs`: направленный event-feed (AppEvent + mpsc), `drain_events` в tick; дельта-синк UI (`last_ui: UiState`) — тик пишет только изменившееся, на паузе/стопе сикбар не дёргается.
- 4.5 Сикбар: TouchArea(grab) поверх Slider (`top_panel.slint`), `seek-dragging`/`pending-seek`, callback `seek-commit` → единый seek при отпускании; тик при драге не трогает seek-fraction/pos/dur.
- 4.6 Трей-громкость → UI-ползунок (пассивно, через дельта-синк тика ≤100 мс).
- 4.7 `disk_tracks: Vec<Track>`: порядок диска (load + scan) ≠ view-порядок; `save_playlist()` пишет disk_tracks; сохранение плейлиста — только при выходе (`playlist_dirty`), eager-save убраны из scan/remove/clear/sort.
- 4.4 Семантика настроек: eager-`settings.save()` убраны (volume GUI+wheel, shuffle, repeat, sort prefs, column widths); save — при выходе через `save_window_geometry()` (оба выхода); `on_settings_save` — diff-apply с переживанием live-полей (volume/muted/last_dir/repeat/shuffle/sorted_col/sort_desc/win_*).

## Шаги (итог)

- [x] 1.1–1.3, 2.2, 2.3, 3.2, 3.3, 4.1–4.7 (все закрыты; коммиты в git log)

## Следующий ход

- Свериться: `git status --porcelain` чистый (кроме незакоммиченного ROADMAP/_STATE_ этой записи).
- Будущие задачи: F1 визуализация (после папки с Python-примером), F2 хоткеи, F3 wheel-громкость в окне, F4 тесты менеджеров `app/`.

## Изменяемые файлы (текущий шаг)

- `src/app/mod.rs` (on_settings_save diff-apply), `src/app/playlist_manager.rs`, `src/app/playback_manager.rs`, `src/app/ui_manager.rs`, `src/app/events.rs`, `ui/top_panel.slint`, `ui/app.slint`

## Риск / стоп-условие

- Сохранение настроек завязано на `save_window_geometry()` на выходе: если добавится новый выход из приложения — не забыть его.
- `remove_track` синхронизирует disk_tracks по пути; при добавлении «одинаковых» путей возможна потеря записи при удалении одной из них — следить.