# Документация APAP

## Структура

| Путь | Что это | Статус |
|---|---|---|
| `docs/01_audio_modes_v1.0/` | Аудио-тракт: три режима воспроизведения, бейдж bit-perfect, «Тест», окно «Путь сигнала», Exclusive на Linux, DSD, пропуск треков. Префикс `AM1.0`. | **действующая**, реализуется по этапам С0…С11 |
| `docs/02_settings_persistence_v1.0/` | Хранение и применение настроек: `settings.toml` / `state.toml`, единственный писатель, отложенная запись, путь выхода, окно сообщений, громкость и колёсико, порядок плейлиста. Префикс `SP1.0`. | **действующая**, реализуется по этапам С0…С12 (сквозной порядок с AM1.0 — `ROADMAP.md`) |
| `docs/spec_visualizer_v5.1.md` | Визуализация (спектр, осциллограмма, спектрограмма). Префикс `V5.1`. Разделы §7–§8 (bit-perfect, DSD→PCM, ресемплинг, выбор устройства) заменены `docs/01_audio_modes_v1.0/` — пометка в начале файла. | действующая (кроме §7–§8) |
| `docs/spec_theme_v1.0.md` | Темы из TOML-файлов. Префикс `T1.0`. | действующая, реализована |
| `docs/acceptance/` | Пакеты кросс-ОС приёмки для `tools/acceptance.py`. | инструмент |
| `docs/_template/` | Шаблон промта и порядок работы для новой задачи. | шаблон |
| `docs/_canceled/` | Архив заменённой и устаревшей документации. **Не источник требований**; агент не читает его без прямой просьбы пользователя. | архив |

### Папка задачи `docs/NN_имя_vX.Y/`

`NN` — порядковый номер, `имя` — латиницей через `_`, `vX.Y` — версия. Состав (на примере `01_audio_modes_v1.0`):

| Файл | Содержание |
|---|---|
| `prompt_review.md` | промт: требования задачи и формат ответа |
| `01_review.md` | ревью кода (R-N), решения пользователя, поправки к ревью |
| `02_tz.md` | ТЗ: требования ТЗ-N, открытые вопросы ОВ-N и решения по ним |
| `03_spec.md` | спецификация: ADR-N, модель данных, алгоритмы, инварианты И-Т/И-Р, план тестов, этапы СN (§8), ОВС-N и решения |
| `prompt_<тема>.md`, `<тема>_test.md` | вспомогательный эксперимент и его результат (здесь — `prompt_pw_reserve.md`, `pw_reserve_test.md`) |

Код ссылается на `ТЗ-N` / `ADR-N` / `§X.Y` действующей спецификации; `ROADMAP.md` — на этапы (`AM1.0-8.N` = этап СN).

## Как начать новую задачу

1. Прочитай `docs/_template/process.md`.
2. Создай `docs/NN_имя_vX.Y/` и скопируй туда `docs/_template/prompt_template.md` как `prompt_review.md`; заполни раздел «Требования задачи».
3. Дальше — по шагам `process.md`: ревью → решения → ТЗ → решения по ОВ → спецификация → решения по ОВС → этапы в `ROADMAP.md` → реализация.

## Перенесённые документы (2026-09-25)

Коды в комментариях кода (`ТЗ A2.0 §5.2`, `ТЗ A3.0 §7.3`, `AP5.0`) указывают на документы из правого столбца.

| Старый путь | Новый путь | Код |
|---|---|---|
| `review/prompt_review.md` | `docs/01_audio_modes_v1.0/prompt_review.md` | `AM1.0` |
| `review/prompt_pw_reserve.md` | `docs/01_audio_modes_v1.0/prompt_pw_reserve.md` | |
| `review/pw_reserve_test.md` | `docs/01_audio_modes_v1.0/pw_reserve_test.md` | |
| `review/01_review.md` | `docs/01_audio_modes_v1.0/01_review.md` | |
| `review/02_tz.md` | `docs/01_audio_modes_v1.0/02_tz.md` | |
| `review/03_spec.md` | `docs/01_audio_modes_v1.0/03_spec.md` | |
| `review/spec_promt.md` | `docs/01_audio_modes_v1.0/spec_promt.md` | |
| `docs/spec_audio_core_v2.0.md` | `docs/_canceled/spec_audio_core_v2.0.md` | `A2.0` |
| `docs/spec_audio_settings_v3.0.md` | `docs/_canceled/spec_audio_settings_v3.0.md` | `A3.0` |
| `docs/spec_audio_pipeline_v5.0.md` | `docs/_canceled/spec_audio_pipeline_v5.0.md` | `AP5.0` |
| `docs/reviews/*.md` (4 файла) | `docs/_canceled/reviews/*.md` | |
| `_TODO_/README.md` | `docs/_canceled/_TODO_/README.md` | |
| `_TODO_/done/**` (18 файлов) | `docs/_canceled/_TODO_/done/**` | |
| `_TODO_/done/мои замечания. txt` | `docs/_canceled/_TODO_/done/мои замечания. txt` | |
