<!-- GENERATED FILE — do not edit by hand.
     Source of truth: _STATE_.yaml — edit that, then run:
     python tools/state_tool.py render -->

# Состояние сессии

- **Текущая задача:** Нет (все шаги завершены)
- **Состояние:** done

## План: Executable workflow: правила AGENTS.md → исполняемые механизмы
_Источник: чат с пользователем (ноутбук), начат в 67686ce; перенесён в репо 2026-09-22_

[x] 1. _STATE_.md → schema/YAML + generated Markdown — 436a681
[x] 2. Автоматическая проверка STATE ↔ Git whitelist — 3f0cfaf
[x] 3. Автоматическая traceability-проверка SPEC ↔ ROADMAP — 1fefed3
[x] 4. Verification scripts для performance requirements (≤5% CPU, ≤50 ms, 0 allocations, DSD512 no OOM) — 06db033, 89ce5b8, 6a4f747; провал DSD512 CPU-бюджета вынесен в ROADMAP V5.1-11.1
[~] 5. Кросс-ОС приёмка спеки (вместо CI): tools/acceptance.py run/status, результаты по ОС в git, прогон при закрытии спеки, не на каждой задаче — инструмент готов; ждёт прогонов пакета baseline-2026-09 на linux и windows (python tools/acceptance.py status)
[x] 6. Убрать ссылки из docs/ на запрещённый _DRAFTS_ — ссылки удалены; traceability_tool.py check #6 не даёт вернуть
[x] 7. Сделать ROADMAP автоматически валидируемым — 1fefed3 (префиксы, якоря, коммиты) + проверки #7-10 traceability_tool: форма таблиц, уникальные ID, словарь статусов/приоритетов, ссылки из _STATE_ и acceptance known_issue
[>] 8. Согласовать единую спеку аудио-тракта docs/spec_audio_pipeline_v5.0.md (AP5.0) — ТЕКУЩАЯ ОСНОВНАЯ ЗАДАЧА — Пользователь читает спеку и даёт уточнения (читать только её; §4-5 политики и «Дополнительно», §24 открытые вопросы, §25 история концепции). Принято 2026-09-22: DSD-fallback фиксирован Native → DoP → PCM, переставлять нельзя, можно только отключать пути. Ждут ответа: открытые вопросы §24 (8 шт., особенно 1 формат DoP, 2 allow_fallback, 8 SignalPath при Shared); оставлять ли docs/reviews/ как историю. После утверждения: статус спеки «Утверждена»; удалить _TODO_/done/ap5.0-sources/ (исходники спеки; НЕ разбирать их в ROADMAP); разнести §22 в ROADMAP (префикс AP5.0); пакет приёмки docs/acceptance/ap5.0 из §23.

Легенда: [x] сделано · [~] частично · [>] в работе · [ ] не начато
