Не читай и не применяй `AGENTS.md`, `CLAUDE.md`, `_STATE_.md`, `_STATE_.yaml`, `ROADMAP.md` и каталоги `docs/_canceled/`, `_DRAFTS_/`. Работай только в `tools/wheel_probe/`. Код плеера (`src/`, `ui/`, `build.rs`, корневые `Cargo.toml` и `Cargo.lock`) не меняй.

# Задача: собрать и запустить wheel_probe на этой машине

`tools/wheel_probe/` — самостоятельный Rust-крейт. Он записывает сырые события колёсика мыши и тачпада. Колёсиком и тачпадом действует **пользователь**; твоя задача — подготовить среду, собрать программу, запустить её, дождаться завершения и закоммитить результаты. Результаты ничего не анализируй и не нормализуй: анализ делается в другой сессии.

## 1. Определи среду

- ОС и версия. Linux: `cat /etc/os-release`, `uname -r`. Windows: `ver`. macOS: `sw_vers`.
- Рабочий стол. Linux: `echo $XDG_CURRENT_DESKTOP`.
- X11 или Wayland. Linux: `echo $XDG_SESSION_TYPE`.

Сообщи пользователю одной строкой, что определено.

## 2. Проверь зависимости и подскажи, что установить

Сам ничего не устанавливай с правами администратора: дай пользователю точную команду или инструкцию.

- **Rust toolchain.** `cargo --version`. Если его нет — https://rustup.rs (Windows: `rustup-init.exe`; Linux и macOS: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`).
- **Windows.** Нужны Microsoft C++ Build Tools (Visual Studio Installer → «Desktop development with C++»). Проверка: `where link` находит `link.exe` MSVC из Developer Command Prompt.
- **macOS.** Нужны Xcode Command Line Tools: `xcode-select -p`, установка — `xcode-select --install`.
- **Linux.** Нужны системные пакеты для сборки Slint (winit, программный рендерер) — пакеты разработчика `fontconfig` и `libxkbcommon`, а также компилятор C.
  - Точный перечень для вашей версии Slint — в документации сборки Slint: https://github.com/slint-ui/slint/blob/master/docs/building.md (раздел Linux; ссылка на master, соответствие версии 1.17.1 не проверялось).
  - Если сборка падает на отсутствующей системной библиотеке — подскажи пакет, названный в ошибке.
- **GNOME (Linux) — трей.** Нужно расширение AppIndicator/KStatusNotifierItem Support (`appindicatorsupport@rgcjonas.gmail.com`).
  - Проверка: `gnome-extensions list --enabled | grep -i appindicator`.
  - Если не найдено — попроси пользователя установить и включить расширение (пакет дистрибутива, например `gnome-shell-extension-appindicator`, или https://extensions.gnome.org/extension/615/appindicator-support/), затем перелогиниться.
  - Без него шаги трея будут без событий: программа покажет это в строке статуса, пользователь пропускает эти шаги.

## 3. Собери

```sh
cd tools/wheel_probe
cargo build --release
```

**Версии крейтов не меняй.** Они обязаны совпадать с плеером (`=1.17.1`, `=0.3.6`, `=1.53.1` и `Cargo.lock`).

Не запускай `cargo update` и не меняй `Cargo.toml` или `src/`. При ошибке диагностируй её (системная библиотека, toolchain, переменные окружения). Если без изменения кода или версий не собрать — остановись и запиши:
- в `tools/wheel_probe/results/<os>_build_<desktop>/build_error.txt`: полный вывод `cargo build --release`, `cargo --version`, `rustc --version`, ОС;
- затем закоммить этот файл, как в п. 6.

## 4. Запусти и дождись завершения

```sh
cargo run --release
```

Скажи пользователю: **«Следуйте шагам в окне; по завершении закройте окно».** Жди, пока процесс не завершится (пользователь закроет окно). Не завершай процесс сам.

## 5. Проверь результаты

В `tools/wheel_probe/results/` появился новый каталог `<os>_<backend>_<desktop>` (например, `windows_win32_windows`). В нём должны быть `env.txt`, `summary.txt` и файлы `NN_*.csv`.

Покажи пользователю содержимое `summary.txt` целиком. Если каких-то файлов нет — скажи, каких.

## 6. Закоммить и отправь

```sh
git checkout -b wheel-results-<os>-<desktop>
git add tools/wheel_probe/results/<каталог>
git commit -m "wheel_probe: результаты <os> <desktop> <session>"
git push -u origin wheel-results-<os>-<desktop>
```

Вместо `<os>`, `<desktop>`, `<session>` подставь значения из имени каталога и `env.txt` (например, `windows windows win32`).

Коммить только каталог результатов. Если `git push` недоступен (нет прав или сети) — покажи пользователю эти команды, чтобы он выполнил их сам.

## 7. Ничего не анализируй

Не делай выводов о величине щелчка, знаках и нормализации: этим занимается основная сессия по `docs/02_settings_persistence_v1.0/wheel_test.md`.
