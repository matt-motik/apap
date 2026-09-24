# Проверка: возврат ЦАП в PipeWire после монопольного доступа (pw-reserve)

Дата: 2026-09-24, Linux-десктоп (Manjaro, ядро 6.18.49), PipeWire/WirePlumber 1.6.8.
Воспроизводилась только тишина (`/dev/zero`). Громкость и конфиги не менялись. Один перезапуск wireplumber+pipewire (после шага 4, для восстановления).

## Параметры

| Параметр | Значение |
|---|---|
| Карта | 4 — `DAC56680121 [ADI-2 DAC (56680121)]` |
| Устройство | `hw:4,0` |
| Формат / частота / каналы | `S32_LE`, 48000 Hz, 2 |
| Имя резервирования | `org.freedesktop.ReserveDevice1.Audio4` |

## Шаг 0. Подготовка

```
$ cat /proc/asound/card4/stream0
Playback: Format: S32_LE  Channels: 2
  Rates: 44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000

$ busctl --user list | grep ReserveDevice
org.freedesktop.ReserveDevice1.Audio0..Audio4   1056 wireplumber   # все карты зарезервированы WirePlumber

$ which pw-reserve
/usr/bin/pw-reserve

$ wpctl status   (фрагмент)
 ├─ Devices:  96. ADI-2 DAC (56680121) [alsa]
 ├─ Sinks:  * 98. ADI-2 DAC (56680121) Analog Stereo [vol: 1.00]

$ fuser -v /dev/snd/*C4*
/dev/snd/controlC4:  matt 1056 F.... wireplumber     # PCM закрыт, sink в suspend
/proc/asound/card4/pcm0p/sub0/status: closed
```

## Шаг 1. Контрольный прогон (без резервирования)

```
$ aplay -D hw:4,0 -f S32_LE -r 48000 -c 2 -d 5 /dev/zero
aplay: main:850: audio open error: Device or resource busy

$ fuser -v /dev/snd/*C4*          # сразу и через 10 с — одинаково
/dev/snd/controlC4:  matt 1056 F.... wireplumber
/dev/snd/pcmC4D0c:   matt 1054 F...m pipewire
/dev/snd/pcmC4D0p:   matt 1054 F...m pipewire
```

Причина занятости: между шагом 0 и шагом 1 PipeWire вышел из suspend и открыл PCM. Его держат
stream-узлы захвата `ADI-2 …:monitor_FL/FR` и `capture_FL/FR`, их владелец —
`plasmashell` (`application.id = org.kde.plasma-pa`), то есть индикаторы уровня
(peak meters) апплета громкости KDE. Они захватывают monitor/capture всех карт.
Ждал 90 с, пока PCM закроется: состояние так и осталось `RUNNING`. Повторить
контрольный прогон на suspend-карте не удалось, а трогать plasmashell я не стал.

ADI-2 из Sinks не пропадал (aplay не получил устройство), перезапуск не понадобился.

### Шаг 1, повтор (апплет громкости закрыт, sink в suspend)

```
Перед прогоном: /proc/asound/card4/pcm0p/sub0/status → closed
                fuser → /dev/snd/controlC4: wireplumber (PCM никем не открыт)

$ aplay -D hw:4,0 -f S32_LE -r 48000 -c 2 -d 5 /dev/zero
Playing raw data '/dev/zero' : Signed 32 bit Little Endian, Rate 48000 Hz, Stereo
exit=0

Сразу и через 10 с (одинаково):
  wpctl  → Devices: 139. ADI-2 DAC [alsa]; Sinks: * 138. ADI-2 DAC Analog Stereo; Sources: 137
  fuser  → /dev/snd/controlC4: wireplumber
  busctl → ReserveDevice1.Audio4 → wireplumber (1056)

Проверка, что PipeWire может снова открыть карту (3 с тишины):
$ timeout 3 pw-cat -p --raw --format s32 --rate 48000 --channels 2 --target 138 - < /dev/zero
  во время воспроизведения: pcmC4D0p → pipewire (1054), state: RUNNING; exit=0
```

Прямое открытие `hw:` приостановленной карты прошло, ADI-2 из Sinks не пропадал, после
закрытия aplay PipeWire штатно открыл PCM.

## Шаг 2. Прогон с резервированием

```
$ timeout 30 pw-reserve -n Audio4 -r -a apap-test   (фон)
device Audio4 is busy
doing RequestRelease on Audio4
reserve available org.freedesktop.ReserveDevice1.Audio4
reserve acquired
... (timeout через 30 с)
doing Release on Audio4

+2 с: busctl → org.freedesktop.ReserveDevice1.Audio4  8468 pw-reserve
      wpctl status → ADI-2 отсутствует полностью (ни в Devices, ни в Sinks/Sources)
      fuser → на /dev/snd/*C4* никого

$ aplay -D hw:4,0 -f S32_LE -r 48000 -c 2 -d 5 /dev/zero
Playing raw data '/dev/zero' : Signed 32 bit Little Endian, Rate 48000 Hz, Stereo
exit=0

Сразу после снятия резервирования:
  busctl → ReserveDevice1.Audio4  1056 wireplumber   (WirePlumber забрал имя обратно)
  wpctl  → Devices: 139. ADI-2 DAC (56680121) [alsa]  (Sinks ещё нет)
  fuser  → /dev/snd/controlC4: wireplumber

Через 10 с:
  wpctl  → Sinks: * 138. ADI-2 DAC (56680121) Analog Stereo [vol: 1.00]
           Sources:  137. ADI-2 DAC (56680121) Analog Stereo
  fuser  → pcmC4D0p/pcmC4D0c: pipewire (снова открыты для peak meters plasma-pa)
```

## Шаг 3. Журнал

В `journalctl --user -u wireplumber -b --since "15 min ago"` нет строк про резервирование и card 4.
Шаг 2 дал однозначный результат, так что журнал для вывода не нужен.

## Шаг 4. Конфликт: PipeWire обращается к карте, пока её держит прямой hw: (без резервирования)

Перед прогоном: PCM `closed` (sink в suspend), `controlC4` держит только WirePlumber.

```
21:10:23  aplay -D hw:4,0 -f S32_LE -r 48000 -c 2 -d 30 /dev/zero          (фон, держит PCM 30 с)
21:10:26  timeout 10 pw-cat -p --raw --format s32 --rate 48000 --channels 2 --target 138 - < /dev/zero

Во время конфликта (+6 с):
  fuser  → /dev/snd/pcmC4D0p: aplay
  wpctl  → Devices: 139. ADI-2 DAC [alsa]; Sinks: ADI-2 ОТСУТСТВУЕТ; Sources: 143. ADI-2 …

Журнал:
pipewire[1054]: spa.alsa: 'front:4': playback open failed: Device or resource busy   (×4)
pipewire[1054]: pw.node: (alsa_output.usb-RME_ADI-2_DAC__56680121__1EE9B453349CCC8-00.analog-stereo-138)
                suspended -> error (Start error: Device or resource busy)
pipewire[1054]: mod.adapter: …: can't get format: Device or resource busy
wireplumber[1056]: s-monitors: Failed to create ALSA node
                alsa_output.usb-RME_ADI-2_DAC__56680121__1EE9B453349CCC8-00.analog-stereo:
                Object activation aborted: PipeWire proxy destroyed

aplay exit=0 (отыграл 30 с), pw-cat exit=124 (timeout)

Сразу, через 10 с и через ещё 30 с после освобождения карты aplay-ем:
  wpctl  → Devices: 139. ADI-2 DAC [alsa]; Sinks: ADI-2 ОТСУТСТВУЕТ; Sources: 143. ADI-2 …
  fuser  → /dev/snd/controlC4: wireplumber (PCM свободен, но sink не пересоздаётся)
  busctl → ReserveDevice1.Audio4 → wireplumber
```

**Эффект воспроизведён**: ADI-2 пропал из Sinks и сам не вернулся. Для восстановления понадобился
`systemctl --user restart wireplumber pipewire`.

Механизм такой. Пока карту держит прямой `hw:`, приходит поток на sink ADI-2 (любое приложение,
системный звук, peak meters). PipeWire выводит узел из suspend и пытается открыть PCM, получает
`EBUSY`, узел уходит в `error`. WirePlumber уничтожает его и пытается создать заново, снова ловит
`EBUSY` (`Failed to create ALSA node`) и больше не повторяет. После освобождения карты
никакого события, по которому WirePlumber повторил бы попытку, нет: ALSA-устройство не исчезало,
резервирование на D-Bus всё время было у WirePlumber.

Для сравнения, в шаге 2 при захвате через `ReserveDevice1` WirePlumber сам снимает узлы ADI-2.
Потокам некуда обращаться к карте, `EBUSY` не возникает, а после `Release` узлы создаются заново.

### Как подтверждена ошибка (подробно)

**Постановка.** Прямой `hw:` без резервирования, как у плеера. От повтора шага 1, где ошибки
не было, эксперимент отличается ровно одним: пока карту держит aplay, в sink ADI-2 пишет
поток PipeWire. Так воспроизводится реальная ситуация плеера, который держит ЦАП минутами,
а за это время на sink приходит любой поток (системный звук, браузер, peak meters KDE).

**Исходное состояние (перед 21:10:23):**
- sink `138. ADI-2 DAC Analog Stereo` есть в Sinks;
- PCM `closed` (suspend), у WirePlumber открыт только `controlC4`;
- `ReserveDevice1.Audio4` принадлежит WirePlumber.

**Хронология:**

| Время | Событие | Наблюдение |
|---|---|---|
| 21:10:23 | aplay открыл `hw:4,0`, тишина 30 с | `pcmC4D0p` → aplay |
| 21:10:26 | `pw-cat --target 138` начал писать тишину в sink ADI-2 | PipeWire выводит узел 138 из suspend |
| 21:10:26 | PipeWire открывает `front:4` | `playback open failed: Device or resource busy` ×4 |
| 21:10:26 | узел 138 переходит в ошибку | `suspended -> error (Start error: Device or resource busy)` |
| 21:10:27 | WirePlumber уничтожает узел и пытается создать заново | `Failed to create ALSA node … Object activation aborted` — повторная попытка тоже упала на `EBUSY` |
| 21:10:29 (+6 с) | проверка во время удержания | Sinks: **ADI-2 нет**; Devices: 139 есть; Sources: 143 есть |
| 21:10:36 | pw-cat завершился по timeout (exit=124) | звук никуда не вышел |
| 21:10:53 | aplay закончил (exit=0) и закрыл PCM | карта **свободна**: на `/dev/snd/pcmC4*` никого |
| сразу после | проверка | Sinks: **ADI-2 нет** |
| +10 с | проверка | Sinks: **ADI-2 нет** |
| +10…+40 с | опрос `wpctl status` каждые 2 с, 15 раз | Sinks: **ADI-2 нет ни разу** |
| ≈ +45 с | `systemctl --user restart wireplumber pipewire` | — |
| после рестарта | проверка | Sinks: `*62. ADI-2 DAC Analog Stereo`; `pw-cat` 3 с тишины → PCM RUNNING, exit=0 |

**Что именно доказано:**
1. **Sink ADI-2 не возвращается сам, даже при увеличенном ожидании.** Больше 40 с после
   освобождения карты он отсутствовал во всех проверках, хотя PCM был свободен, устройство
   (`Devices: 139`) оставалось на месте, а резервирование на D-Bus было у WirePlumber.
2. **Вернуть его удалось только перезапуском** `wireplumber` и `pipewire`. После перезапуска sink
   создан заново (id 62) и играет.
3. **Ошибку вызывает именно конфликт `EBUSY` при удержании.** Тот же aplay без встречного
   потока (повтор шага 1) ничего не ломает. С резервированием (шаг 2) встречного обращения
   PipeWire к карте нет, и узлы восстанавливаются сами.
4. **WirePlumber после неудачной попытки больше ничего не делает.** В журнале ровно одна
   попытка пересоздать узел (21:10:27), затем тишина. Повторных попыток в момент освобождения
   карты не было.

**Воспроизводимость.** Шаг 4 выполнен **один раз**, ошибка случилась с первой попытки (1 из 1).
Эффект детерминирован самим механизмом: каждое обращение PipeWire к карте, занятой прямым `hw:`,
даёт `EBUSY`, а после `EBUSY` при старте узел уходит в `error` и WirePlumber его не
пересоздаёт. Условие срабатывания ясное: хотя бы один поток на sink ADI-2 за время прямого
удержания. Серией повторных прогонов стопроцентная воспроизводимость не проверялась.
Каждый прогон требует перезапуска wireplumber/pipewire.

## Шаг 5. Тот же конфликт, но с резервированием через D-Bus

Повторяет шаг 4, отличие одно: перед открытием `hw:` карта захвачена через
`org.freedesktop.ReserveDevice1.Audio4` (`pw-reserve -r`, RequestRelease → acquire), как это
должен делать плеер. Поток PipeWire адресован sink'у ADI-2 **по постоянному имени узла**
(`alsa_output.usb-RME_ADI-2_DAC__56680121__1EE9B453349CCC8-00.analog-stereo`), то есть
конкретно в этот ЦАП.

Исходное состояние: sink `*62. ADI-2` (default), PCM `closed`, `Audio4` → wireplumber (10985).

| Время | Событие | Наблюдение |
|---|---|---|
| 21:24:16 | `timeout 40 pw-reserve -n Audio4 -r -a apap-test` | `device Audio4 is busy` → `RequestRelease` → `reserve acquired` |
| +2 с | проверка | `Audio4` → **pw-reserve**; в `wpctl status` ADI-2 нет совсем (Devices/Sinks/Sources); `/dev/snd/*C4*` свободны, даже `controlC4` |
| 21:24:18 | aplay открыл `hw:4,0`, тишина 30 с | открылся без ошибок |
| 21:24:21 | `pw-cat --target <имя узла ADI-2>` 10 с тишины | узла ADI-2 нет, поток ушёл в fallback: `alsa_output.pci-0000_00_1f.3.analog-stereo` (Built-in Audio) |
| 21:24:24 | окно конфликта | `pcmC4D0p` → только aplay; к карте PipeWire **не обращался** |
| 21:24:31 | pw-cat завершился по timeout (exit=124) | — |
| 21:24:48 | aplay закончил, exit=0 | — |
| 21:24:56 | timeout снял резервирование (`doing Release on Audio4`) | `Audio4` сразу снова → wireplumber (10985); Devices: `79. ADI-2 DAC` |
| ≈ +2 с | опрос каждые 2 с | **Sink ADI-2 вернулся** (id 62), Sources — 77 |
| после | проверка | `@DEFAULT_AUDIO_SINK@` = ADI-2 (`*62`); `pw-cat` 3 с тишины → `pcmC4D0p` → pipewire, RUNNING, exit=0 |

Журнал (`journalctl --user -b --since 21:24:16`, pipewire/wireplumber): **пусто**. Нет ни одного
`Device or resource busy`, `-> error` или `Failed to create ALSA node`.
Перезапуск wireplumber/pipewire **не понадобился**.

**Почему работает.** По `RequestRelease` WirePlumber не просто отдаёт имя на D-Bus, а полностью
снимает ALSA-устройство ADI-2 (Device, Sinks, Sources) и закрывает даже `controlC4`. Пока
резервирование у плеера, в PipeWire нет узла, через который кто-то мог бы открыть карту.
Потоки уходят на другой sink, `EBUSY` не возникает, в `error` уходить нечему. После `Release`
WirePlumber получает имя обратно и создаёт устройство заново, как при подключении.

**Ограничения проверки:**
- прогон один (1 из 1);
- не проверены: аварийное завершение владельца резервирования (по протоколу имя на D-Bus
  освобождается при выходе процесса), отказ в резервировании (другой владелец с более высоким
  приоритетом), а также приложения, открывающие `hw:` напрямую без D-Bus (JACK, второй
  bit-perfect плеер): от них резервирование не защищает;
- побочный эффект, ожидаемый по протоколу: пока ЦАП зарезервирован, звук других приложений,
  адресованный ADI-2, уходит на fallback-sink (здесь Built-in Audio) и может неожиданно
  прозвучать из другого устройства.

## Сводная таблица

| | Пропал из Sinks | Вернулся сам |
|---|---|---|
| Шаг 1 (без резервирования), 1-й прогон | — (aplay: **busy**, PCM держали peak meters plasma-pa) | n/a |
| Шаг 1, повтор (sink в suspend) | **нет** | **да** (не пропадал; PipeWire потом открыл PCM штатно) |
| Шаг 4: hw: без резервирования + поток в PipeWire во время удержания | **да** (sink → `error`, узел уничтожен) | **нет**: не было >40 с после освобождения карты; вернул только restart wireplumber+pipewire |
| Шаг 5: hw: **с** резервированием D-Bus + поток на ADI-2 во время удержания | **да** (штатно, WirePlumber снял устройство) | **да**, ≈2 с после `Release`, без перезапуска; в журнале ни одной ошибки |
| Шаг 2 (pw-reserve) | **да** (штатно, при резервировании) | **да**, без перезапуска: Device сразу, Sinks/Sources ≤10 с |

## Вывод

**Гипотеза подтверждена** (шаг 4 против шага 2): ЦАП не возвращается, когда плеер открывает `hw:`
в обход `org.freedesktop.ReserveDevice1` и PipeWire во время удержания пытается открыть карту.
Sink ADI-2 пропал, не появился сам больше чем за 40 с после освобождения карты, вернул его только
`systemctl --user restart wireplumber pipewire`. Ошибка случилась с первой попытки (1 из 1),
подробности — в разделе «Как подтверждена ошибка».

1. Одного прямого открытия мало. Если за время удержания PipeWire к карте не обращался
   (повтор шага 1: 5 с, sink в suspend), всё проходит незаметно. У плеера удержание долгое, и за это
   время почти наверняка придёт какой-нибудь поток на sink ADI-2 или откроются peak meters KDE.
2. Если PipeWire уже держит PCM (1-й прогон шага 1), прямое открытие сразу падает с `EBUSY`.
3. С резервированием (шаг 2) WirePlumber по `RequestRelease` снимает узлы карты даже во время
   активных потоков. Конфликта `EBUSY` нет, а после `Release` Device и Sinks/Sources
   восстанавливаются сами за ≤10 с.
4. **Резервирование через D-Bus устраняет проблему** в том же сценарии (шаг 5): карта отдана
   плееру, PipeWire к ней не обращается, после `Release` sink возвращается сам за ≈2 с.
   Ограничения проверки и побочные эффекты описаны в шаге 5.
5. **Что делать в плеере:** перед открытием `hw:N` захватывать `org.freedesktop.ReserveDevice1.AudioN`
   (RequestRelease → acquire, как `pw-reserve -r`), держать имя всё время удержания и
   освобождать после закрытия PCM. Если резервирование отклонено (владелец не отдаёт карту), не
   открывать `hw:` и сообщать об ошибке вместо тихого захвата.

## Итоговое состояние системы

После шага 4 выполнен `systemctl --user restart wireplumber pipewire` (восстановление по регламенту).

```
Devices: 53. ADI-2 DAC (56680121) [alsa]
Sinks:  *62. ADI-2 DAC (56680121) Analog Stereo [vol: 1.00]
Sources: 61. ADI-2 DAC (56680121) Analog Stereo
busctl: org.freedesktop.ReserveDevice1.Audio4 → wireplumber (10985)
pw-cat 3 с тишины на sink 62: pcmC4D0p → pipewire, state RUNNING, exit=0
```

ADI-2 в Sinks, звук через PipeWire работает.
