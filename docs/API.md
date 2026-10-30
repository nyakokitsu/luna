# HTTP API Luna

Этот документ описывает интерфейс, через который Luna и пакеты работают со
станцией. API специально не повторяет устройство: пакет вызывает одну и ту же
функцию на разных моделях, а station crate выбирает нужный backend.

Перед вызовом проверяйте capability через `/api/device` или соответствующий
`is_available()`. Если возможности нет, Luna возвращает понятную ошибку, а не
пытается записать данные в чужой sysfs-путь.

Примеры используют адрес станции:

```sh
BASE=http://<IP Станции>:8099/api
```

Для JSON-запросов нужен заголовок:

```text
Content-Type: application/json
```

Успешные синхронные операции возвращают `200 OK`. Запуск пакета, который
продолжается в фоне, возвращает `202 Accepted`.

При ошибке API возвращает `400 Bad Request` и JSON:

```json
{"error":"описание ошибки"}
```

## Медиапровайдеры

`GET /api/media` возвращает выбранный пакет, доступность провайдеров и результат
последней операции. `POST /api/media/select` принимает `{"provider":"radio"}`,
`POST /api/media/command` — `{"action":"pause"}`. Изменения возвращают 202 и ID;
результат проверяется через GET. Переключение само не запускает музыку.

Регистрация, подтверждение команд и ограничения описаны в
[MEDIA_PROVIDERS.md](MEDIA_PROVIDERS.md). Это API адаптеров пакетов, без встроенного
голосового перехвата и без адаптера штатного плеера.

## Устройство и настройки

### `GET /api/device`

Возвращает модель, архитектуру, capabilities и топологию LED.

```sh
curl "$BASE/device"
```

Пример части ответа:

```json
{
  "model": "yandexmidi",
  "architecture": "aarch64",
  "features": ["led.ring.rgb", "led.logo", "display.clock.segment.v1"],
  "source": "auto (override: data-dir/device.toml or --model)",
  "led": {
    "ring_count": 24,
    "has_logo": true,
    "logo_count": 6
  }
}
```

### `GET /api/room`

Возвращает локальную пользовательскую метку станции. Если метка не задана,
возвращается `null`.

```sh
curl "$BASE/room"
```

### `PUT /api/room`

Сохраняет метку в `room.toml` внутри `--data-dir` Luna. Пустая строка или
`null` удаляют её; допустимо до 64 Unicode-символов без управляющих символов.

```sh
curl -X PUT "$BASE/room" \
  -H 'Content-Type: application/json' \
  -d '{"room":"Кухня"}'
```

Ответ имеет вид `{"room":"Кухня"}` или `{"room":null}`. Значение также
появляется в `GET /api/device` и доступно Lua как `device.room`.

### `PUT /api/device`

Сохраняет override профиля устройства в `device.toml`.

```sh
curl -X PUT "$BASE/device" \
  -H 'Content-Type: application/json' \
  -d '{
    "model":"yandexmini3",
    "architecture":"aarch64",
    "features":["input.evdev"],
    "source":"manual",
    "led":{"ring_count":6,"has_logo":false,"logo_count":0}
  }'
```

### `GET /api/settings`

Возвращает текущие настройки Luna.

```sh
curl "$BASE/settings"
```

### `PUT /api/settings`

Полностью заменяет настройки. В JSON должна быть секция `led`.

```sh
curl -X PUT "$BASE/settings" \
  -H 'Content-Type: application/json' \
  -d '{
    "led": {
      "profile":"custom",
      "count":6,
      "sysfs_root":"/sys/class/leds",
      "path_template":null
    },
    "web":{"ui":true,"api":true},
    "allow_plugin_binaries":false,
    "updates":{"enabled":false,"interval_minutes":360},
    "lua": {
      "enabled": true,
      "share_dir": "share/lua",
      "max_runtime_seconds": 30,
      "allow_infinite_loops": false,
      "permissions": {
        "led": true,
        "volume": false,
        "audio": false,
        "microphone": false,
        "bluetooth": false,
        "display": false,
        "sensors": false,
        "clock": false,
        "input": false,
        "network": false,
        "ipc_read": false,
        "ipc_control": false,
        "commands": false,
        "system": false,
        "exec": false
      },
      "network_allow": [],
      "input_devices": [],
      "ipc_services": [],
      "command_phrases": []
    }
  }'
```

`led.count` должен быть в диапазоне `1..128`. Новые неполные
`settings.toml` без `[led]` Luna умеет дополнять defaults.

## Пользовательский Lua без установки пакета

Пользовательские Lua-файлы можно хранить в `<data-dir>/share/lua/`. По
умолчанию для standalone Luna это `/data/quazar/luna/data/share/lua/`. Файл не нужно
упаковывать в `.yap`: Luna запускает его через отдельный user-Lua runner.
Режим отключён по умолчанию и включается через `lua.enabled = true`.

Список файлов:

```sh
curl "$BASE/lua/files"
```

Запуск:

```sh
curl -X POST "$BASE/lua/run" \
  -H 'Content-Type: application/json' \
  -d '{"file":"hello.lua"}'
```

Ответ содержит id запуска:

```json
{"id":"user_lua_...","file":"hello.lua","status":"started"}
```

Остановка:

```sh
curl -X POST "$BASE/lua/user_lua_<id>/stop"
```

Путь ограничен `lua.share_dir`; traversal через `..` и абсолютные пути
запрещены. Выполнение можно полностью отключить через `lua.enabled`.

### Разрешения пользовательского Lua

User Lua получает только разрешения из `lua.permissions`. Все они по умолчанию
выключены:

| Поле | Что разрешает |
|---|---|
| `led` | LED ring/logo |
| `volume` | изменение громкости |
| `audio` | проигрывание audio |
| `microphone` | запись с микрофона |
| `bluetooth` | BlueZ и Bluetooth scan/pair |
| `display` | backlight и display API |
| `sensors` | чтение и управление датчиками |
| `clock` | сегментные часы |
| `input` | чтение input events |
| `network` | HTTP-запросы к `network_allow` |
| `ipc_read` | чтение разрешённых `ipc_services` |
| `ipc_control` | отправка команд разрешённым `ipc_services` |
| `commands` | обработчики разрешённых `command_phrases` |
| `system` | системная read-only информация |
| `exec` | вызов команд через Lua API; дополнительно требует `allow_plugin_binaries` |

`network_allow`, `input_devices`, `ipc_services` и `command_phrases` —
ограничивающие списки. Например, включённый `network` без непустого
`network_allow` не даёт сетевого доступа.

### Ограничение циклов и времени

По умолчанию Lua-код принудительно останавливается после
`lua.max_runtime_seconds` и не может бесконечно крутиться в чистом Lua-цикле.
`allow_infinite_loops = true` отключает лимит времени; ручная остановка через
endpoint всё равно остаётся доступной.

Даже при разрешённых бесконечных циклах рекомендуется использовать
`alice.sleep(...)`, чтобы не загружать CPU станции.

## LED

Тело всех LED set-запросов:

```json
{"colors":["#FF0000","#0000FF"]}
```

Цвета передаются компактным списком. Luna расширяет его на физическое число
LED выбранной зоны: если цветов меньше, они распределяются максимально
равномерно по всей зоне. Количество LED можно узнать через `GET /api/device`.
Если цветов больше, чем LED, API возвращает `400` и не применяет кадр — лишние
цвета не отбрасываются молча.

## Громкость

Текущая громкость:

```sh
curl "$BASE/volume"
```

Ответ:

```json
{"percent":35}
```

Установка громкости от 0 до 100 процентов:

```sh
curl -X POST "$BASE/volume" \
  -H 'Content-Type: application/json' \
  -d '{"percent":20}'
```

При успехе API возвращает `200` и установленное значение. Если ALSA Mixer
недоступен или значение некорректно, API возвращает `400` с JSON вида
`{"error":"..."}`.

Изменение громкости на 5 процентов (шаг по умолчанию):

```sh
curl -X POST "$BASE/volume/up"
curl -X POST "$BASE/volume/down"
```

Свой шаг можно передать в JSON:

```sh
curl -X POST "$BASE/volume/up" \
  -H 'Content-Type: application/json' \
  -d '{"percent":10}'
```

Управление mute:

```sh
curl -X POST "$BASE/volume/mute"
curl -X POST "$BASE/volume/unmute"
```

## Bluetooth

Bluetooth управляется штатным стеком станции. На Quasar-станциях Luna передаёт
включение Bluetooth и режим сопряжения в `maind`/`BluetoothCapability`; Luna не
создаёт отдельный аудиомаршрут и не воспроизводит A2DP через собственный ALSA.
Конкретные операции доступны только если их поддерживает прошивка станции.

Статус радио, сервиса и найденных устройств:

```sh
curl "$BASE/bluetooth/status"
```

Пример ответа:

```json
{
  "available": true,
  "powered": true,
  "discovering": false,
  "pairable": true,
  "server_running": true,
  "power_control_available": false,
  "power_control_reason": "...",
  "devices": []
}
```

Режим обнаружения устройств:

```sh
curl -X PUT "$BASE/bluetooth/scan" \
  -H 'Content-Type: application/json' \
  -d '{"on":true}'
```

Отключить обнаружение:

```sh
curl -X PUT "$BASE/bluetooth/scan" \
  -H 'Content-Type: application/json' \
  -d '{"on":false}'
```

Включить или выключить режим сопряжения станции:

```sh
curl -X PUT "$BASE/bluetooth/pairable" \
  -H 'Content-Type: application/json' \
  -d '{"on":true}'
```

После успешного ответа `200 OK` станция становится доступной для нового
сопряжения. На некоторых прошивках обновление списка Bluetooth на телефоне или
компьютере нужно запустить вручную.

Сопряжение, подключение, отключение и удаление устройства:

```sh
curl -X POST "$BASE/bluetooth/pair" \
  -H 'Content-Type: application/json' \
  -d '{"address":"01:23:45:67:89:ab"}'
curl -X POST "$BASE/bluetooth/connect" \
  -H 'Content-Type: application/json' \
  -d '{"address":"01:23:45:67:89:ab"}'
curl -X POST "$BASE/bluetooth/disconnect" \
  -H 'Content-Type: application/json' \
  -d '{"address":"01:23:45:67:89:ab"}'
```

Питание управляется через `PUT /api/bluetooth/power` с телом
`{"on":true}` или `{"on":false}`. Если функция не поддерживается прошивкой,
API возвращает ошибку вместо ложного успешного ответа.
Перед вызовом Lua-пакет может проверить `alice.is_available("bluetooth.scan")`,
`alice.is_available("bluetooth.pair")` или `alice.is_available("bluetooth.power")`.

## Собственные звуки

Публичного HTTP endpoint для проигрывания файлов нет: звук запускается из Lua
пакета через permission `audio` и передаётся штатному сервису `mediad`.

```toml
permissions = ["audio"]
```

```lua
if alice.audio.is_available() then
  alice.audio.play("sounds/notification.wav")
end
```

Файл должен быть PCM WAV. Luna проверяет RIFF/WAVE-заголовок, число каналов,
частоту, разрядность, размер до 64 MiB и длительность до 600 секунд. MP3 и
другие форматы пакет должен заранее преобразовать в WAV. На станциях Quasar
воспроизведение идёт через `mediad`, без запуска собственного `aplay`.

### Установка цветов

| Метод | Endpoint | Действие |
|---|---|---|
| `POST` | `/api/led` | Ring и logo; если ring отсутствует, управляется logo |
| `POST` | `/api/led/ring` | Только ring |
| `POST` | `/api/led/logo` | Только logo |

```sh
curl -X POST "$BASE/led/ring" \
  -H 'Content-Type: application/json' \
  -d '{"colors":["#FF0000","#0000FF"]}'
```

### Выключение

| Метод | Endpoint | Действие |
|---|---|---|
| `POST` | `/api/led/off` | Выключить ring и logo |
| `POST` | `/api/led/off/ring` | Выключить только ring |
| `POST` | `/api/led/off/logo` | Выключить только logo |

Тело не требуется:

```sh
curl -X POST "$BASE/led/off"
curl -X POST "$BASE/led/off/logo"
```

На Midi после выключения Luna восстанавливает штатные triggers `ledfb-ring`
и `ledfb-logo`, чтобы индикация Алисы снова работала.

## Часы

### `GET /api/clock`

Возвращает доступность часов, список допустимых сегментов и `text_available`. Текст пока поддерживается только sysfs-дисплеем Midi (`cucumber`); отдельные station backends и другие модели возвращают `text_available: false`.

```sh
curl "$BASE/clock"
```

### `POST /api/clock`

Установить время:

```sh
curl -X POST "$BASE/clock" \
  -H 'Content-Type: application/json' \
  -d '{"time":"12:34","brightness":170}'
```

`brightness` — диапазон `0..255`.

Установить отдельные сегменты:

```sh
curl -X POST "$BASE/clock" \
  -H 'Content-Type: application/json' \
  -d '{"segments":{"A0":255,"B0":255,"G0":255}}'
```

Допустим только один режим за запрос: `time` или `segments`.

### `POST /api/clock/text`

Временная надпись на четырёх семисегментных позициях:

```sh
curl -X POST "$BASE/clock/text" \
  -H 'Content-Type: application/json' \
  -d '{"text":"HI","brightness":100,"duration_ms":2000}'
```

`text` — 1–32 ASCII-символа: цифры, пробел, `-`, `_` и буквы
`A B C D E F G H I J L N O P Q R S T U Y` без учёта регистра.
Начертания приближённые; остальные символы и пустая надпись отклоняются.
До четырёх символов центрируются; более длинная строка прокручивается один раз.
`brightness` — `0..100` (по умолчанию 100), `duration_ms` — `1..15000`
(по умолчанию 2000), общее время показа, включая прокрутку.

Запрос завершается после показа или его отмены. Отключение HTTP-клиента не
отменяет возврат дисплея. Новый текст, корректная запись raw/Lua или `/clock/off`
отменяют предыдущий показ. При изменении кадра штатной системой показ уступает
ей управление; иначе возвращается сохранённый кадр. Сбой процесса/SIGKILL не
гарантирует восстановление. API сам не опрашивает медиаплееры.

### `POST /api/clock/off`

Выключает управление часами без тела запроса:

```sh
curl -X POST "$BASE/clock/off"
```

## Микрофоны

Список доступных физических каналов:

```sh
curl "$BASE/microphone/devices"
```

Записать выбранный микрофон в WAV:

```sh
curl -X POST "$BASE/microphone/record" \
  -H 'Content-Type: application/json' \
  -d '{"device":"raw_mic_0","seconds":5}' \
  -o microphone.wav
```

device можно не указывать — тогда используется первый канал. Длительность
ограничена диапазоном 1..30 секунд.

## Речь и локальные команды Алисы

Luna получает распознанные события от локального `aliced`. API не включает
микрофон и не регистрирует собственные wake word или фразы: он только читает
события, которые уже классифицировала прошивка станции.

### `GET /api/alice/speech`

Ожидает следующее событие распознанной речи. STT может присылать несколько
промежуточных состояний по мере появления слов; каждое состояние возвращается
отдельным вызовом endpoint. Таймаут ограничен 60 секундами:

```sh
curl -i -N --max-time 65 \
  "$BASE/alice/speech?timeout_ms=60000"
```

Чтобы после получения речи сразу отменить текущий диалог и очистить очередь
ответов, добавьте `cancel=true`:

```sh
curl -i -N --max-time 65 \
  "$BASE/alice/speech?timeout_ms=60000&cancel=true"
```

Пример ответа для обычной фразы после wake word:

```json
{
  "type": "speech",
  "source": "voice",
  "phase": "listening",
  "request_id": "request-id",
  "transcript": "Привет",
  "final": true
}
```

`final: false` означает промежуточный текст, `final: true` — окончательный
результат распознавания. Для получения полной фразы вызывайте endpoint без
`cancel` и повторяйте запросы. Режим `cancel=true` предназначен для перехвата:
Luna отменяет диалог сразу после первого распознанного фрагмента, пока станция
ещё не отправила запрос и не начала отвечать.

`phase` — текущее состояние Alice (`idle`, `listening`, `busy`, `speaking` или
`shazam`), а `request_id` — идентификатор текущего обращения. Для локальной
команды `source` равен `command`, `phase` — `recognition`, а `request_id` пустой.

### `GET /api/alice/command`

Ожидает событие голосовой команды. В ответе сохраняется `source`:

- `command` — команда распознана локальным command spotter;
- `voice` — обычная фраза голосового диалога, обычно после wake word.

```sh
curl -i -N --max-time 65 \
  "$BASE/alice/command?timeout_ms=60000"
```

Чтобы после получения события отправить в `aliced` отмену текущего диалога,
добавьте `cancel=true`:

```sh
curl -i -N --max-time 65 \
  "$BASE/alice/command?timeout_ms=60000&cancel=true"
```

Для локального события `source=command` это позволяет перехватить команду до
обычного голосового диалога. Для `source=voice` прошивка может уже успеть
отправить запрос в сеть.

### `POST /api/alice/recognize`

Запускает встроенное распознавание станции в режиме `recognition-only`. В этом
режиме Luna получает финальный текст через локальный IPC `aliced`, но не
запускает обычный диалог Алисы и не ожидает ответа из сети:

```sh
curl -i -X POST --max-time 65 \
  "$BASE/alice/recognize?timeout_ms=60000"
```

Ответ имеет тот же формат, что и `/api/alice/speech`, но возвращается только
после финального результата:

```json
{
  "type": "speech",
  "source": "voice",
  "phase": "listening",
  "request_id": "request-id",
  "transcript": "Привет",
  "final": true
}
```

Режим использует штатный распознаватель прошивки. На прошивках без поддержки
`START_RECOGNIZE` финальное событие не появится и endpoint завершится по
таймауту; обычный `/speech` при этом не изменяется.

### `POST /api/alice/speech/cancel`

Отправляет в `aliced` команду отмены текущего голосового диалога и очищает
очередь отложенных ответов. Тело не требуется; при успехе возвращается `200 OK`:

```sh
curl -i -X POST "$BASE/alice/speech/cancel"
```

Этот endpoint можно вызвать сразу после получения локальной команды, чтобы
станция не продолжала обрабатывать её как обычный диалог.

На Mini 1 command spotter содержит, в частности, команды `громче`, `тише`,
`дальше`, `назад`, `пауза`, `стоп`, `продолжи`, `включи свет` и `выключи
свет`. Их следует произносить без слова «Алиса».

## Input и кнопки

### `GET /api/input/devices`

Список доступных evdev-устройств:

```sh
curl "$BASE/input/devices"
```

### Семантические действия Mini 3

На Mini 3 доступны:

| Endpoint | Событие |
|---|---|
| `GET /api/input/volume_up` | Нажатие `+` |
| `GET /api/input/activate` | Нажатие logo/центра |
| `GET /api/input/volume_down` | Нажатие `-` |
| `GET /api/input/mute` | Нажатие mute |

Каждый endpoint ждёт событие. Таймаут задаётся query-параметром:

```sh
curl '$BASE/input/volume_up?timeout_ms=60000'
```

Пример ответа:

```json
{
  "device":"aw933xx",
  "kind":1,
  "kind_name":"EV_KEY",
  "code":115,
  "value":1,
  "value_name":"press",
  "action":"volume_up",
  "time_ms":1793181409377
}
```

### Прямые GPIO-кнопки

На Mini 1 физическая кнопка mute не формирует evdev-событие. Она доступна
через GPIO backend станции:

```sh
curl '$BASE/input/gpio/mute?timeout_ms=60000'
```

Ответ имеет тот же формат input-события:

Для Mini 1 доступны отдельные аппаратные линии:

```sh
curl '$BASE/input/gpio/mute?timeout_ms=60000'
curl '$BASE/input/gpio/unmute?timeout_ms=60000'
```

`mute` отслеживает GPIO414 с falling edge, а `unmute` — GPIO505 с rising
edge.

```json
{
  "device": "gpio-mute",
  "kind": 1,
  "kind_name": "EV_KEY",
  "code": 113,
  "value": 1,
  "value_name": "press",
  "action": "mute"
}
```

### Свайпы Mini 3

| Endpoint | Последовательность |
|---|---|
| `GET /api/input/swipe/minus-to-logo` | `volume_down → activate` |
| `GET /api/input/swipe/plus-to-logo` | `volume_up → activate` |
| `GET /api/input/swipe/minus-to-plus` | `volume_down → activate → volume_up` |
| `GET /api/input/swipe/plus-to-minus` | `volume_up → activate → volume_down` |

Частичные жесты до центральной зоны logo распознаются отдельно: станция может
завершить действие уже на последовательности `кнопка → logo`, не дожидаясь
противоположной кнопки. Полные свайпы остаются отдельными endpoint-ами.

```sh
curl '$BASE/input/swipe/minus-to-plus?timeout_ms=60000'
```

Успешный результат:

```json
{"gesture":"minus-to-plus"}
```

Если полный жест не собран до таймаута:

```json
{"gesture":null}
```

### Сырые input-события

Получить одно событие:

```sh
curl '$BASE/input/event?device=aw933xx&timeout_ms=10000'
```

Получить пачку событий. Endpoint возвращает накопленную последовательность
после паузы `idle_ms` или по достижении `max_events`:

```sh
curl '$BASE/input/events?device=aw933xx&timeout_ms=10000&idle_ms=300&max_events=128'
```

Query-параметры:

- `device` — точное имя evdev-устройства;
- `timeout_ms` — общий timeout, максимум `60000`;
- `idle_ms` — пауза завершения batch после первого события, `20..2000`;
- `max_events` — максимум событий, `1..1024`.

Raw API нужен для диагностики и разработки новых station-specific mappings.

## Датчики

### `GET /api/sensors`

Возвращает найденные датчики температуры, освещённости и батареи:

```sh
curl "$BASE/sensors"
```

## Пакеты

### `GET /api/plugins`

Список установленных `.yap` пакетов и их состояния:

```sh
curl "$BASE/plugins"
```

Плагины, установленные через YAM, возвращаются с `source: "yam"` и полем
`package` (`namespace/name`). Каталог таких плагинов принадлежит YAM: кнопка
удаления Luna для них недоступна, удаление выполняется командой
`yam remove namespace/name`.

#### Binary-плагины из YAM v2

YAM устанавливает и связывает файлы; Luna запускает binary-плагин своим
обычным supervisor и выдаёт ему runtime token. Это не реализация `yam service
start`: в YAM v2 supervisor остаётся отдельной задачей.

Требования:

- `allow_plugin_binaries=true` в Luna и явно включённый в Luna plugin ID.
- `--yam-root`/`YAM_ROOT` и **обязательно** `--yam-run`/`YAM_RUN`, указывающий
  на настоящий run-каталог YAM с существующим `yam.lock`.
- Descriptor в `$YAM_ROOT/share/luna/plugins/namespace.name/plugin.toml`,
  `id='namespace.name'`; выбранная версия —
  `$YAM_ROOT/pkgs/namespace/name/current`.
- Descriptor обязан ссылаться на `share/luna/plugins/namespace.name/plugin.toml`
  внутри выбранной версии. Launcher и явно указанные `library_paths` не могут
  выходить за её plugin-подкаталог. Permissions и IPC/input/command policy
  проверяются до запуска; допустимый interpreter — только `sh`.

Например, runtime-specific descriptor внутри v2 payload:

```toml
id = "example.worker"
name = "Worker"
version = "1.0.0"
entry = "bin/worker"
permissions = ["exec"]
[launch]
kind = "binary"
mode = "service"
# library_paths = ["lib"] # только если такая приватная директория есть
```

Запуск использует immutable plugin-подкаталог выбранной версии как cwd,
не mutable ссылки общего дерева. У YAM binary-плагинов не угадываются старые
apkg-runner/библиотеки; `LD_LIBRARY_PATH` содержит только явно объявленные
приватные директории, не окружение другой установки.

На **всё время выполнения, включая завершение process group**, Luna держит
shared flock на `yam.lock`. Поэтому установка/удаление/repair YAM с exclusive
lock будут отклонены, пока такие плагины работают. Это грубая, но безопасная
граница текущей интеграции: перед любой транзакцией YAM остановите **все**
YAM binary-плагины в Luna, дождитесь завершения, выполните транзакцию и
возобновите нужные плагины. Автоматической orchestration обновлений здесь нет.

Запуск при активной транзакции YAM завершается ошибкой и не ждёт её внутри
async runtime. Отсутствующий lock не создаётся и не обходится. Lua-плагины и
локальные `.yap` сохраняют прежний lifecycle.

Runtime URL использует полный `namespace.name`; token и permissions остаются
привязанными только к этому ID. Логи/state расположены в Luna data-dir:
`plugins/plugin-state/namespace.name/`. Его абсолютный путь передаётся binary
как `LUNA_PLUGIN_STATE`; mutable данные следует писать туда, а не в payload.

Формат `.yampkg` и его `MANIFEST.yaml` по-прежнему проверяет YAM; Luna не
заменяет подпись реестра или БД ownership своим descriptor. Descriptor — лишь
runtime-метаданные внутри уже установленного пакета. Это не OS sandbox.

### `GET /api/plugins/sources`

Показывает обнаруженные каталоги Luna и YAM:

```sh
curl "$BASE/plugins/sources"
```

### `POST /api/plugins/rescan`

Повторно сканирует YAM-каталог и возвращает актуальный список плагинов. Этот
endpoint безопасен для standalone-установки: если YAM не обнаружен, список
остаётся локальным.

### `POST /api/plugins`

Установка локального пакета. Используется multipart-поле `package`:

```sh
curl -X POST "$BASE/plugins" \
  -F 'package=@plugin.yap'
```

### `POST /api/plugins/registry`

Установка пакета из registry:

```sh
curl -X POST "$BASE/plugins/registry" \
  -H 'Content-Type: application/json' \
  -d '{
    "registry":"https://example.org/registry",
    "id":"example.plugin",
    "version":"1.2.0"
  }'
```

`version` можно не указывать: тогда выбирается совместимая максимальная
версия.

### Операции над пакетом

| Метод | Endpoint | Действие |
|---|---|---|
| `DELETE` | `/api/plugins/:id` | Удалить пакет |
| `POST` | `/api/plugins/:id/toggle` | Включить/выключить пакет |
| `POST` | `/api/plugins/:id/invoke` | Запустить вручную |
| `POST` | `/api/plugins/:id/stop` | Остановить |
| `POST` | `/api/plugins/:id/service/enable` | Включить автозапуск |
| `POST` | `/api/plugins/:id/service/disable` | Выключить автозапуск |
| `POST` | `/api/plugins/:id/service/start` | Запустить как service |
| `GET` | `/api/plugins/:id/logs` | Получить runtime log |

Аргументы ручного запуска:

```sh
curl -X POST "$BASE/plugins/example.plugin/invoke" \
  -H 'Content-Type: application/json' \
  -d '{"args":["hello","world"]}'
```

## Runtime API для плагинов

### Разрешение binary-зависимости YAM

`GET /api/runtime/:id/dependencies/:alias` требует `exec`, валидный invocation
token и **активный YAM binary-плагин** (с lifetime lease). Local `.yap` и Lua
не используют этот endpoint: без lease возвращённый путь мог бы быть удалён
транзакцией ещё до вызова. Alias объявляется в runtime descriptor caller:

```toml
[dependencies]
station-volume = "station.volume"
```

Это runtime allowlist, не замена `depends` в исходном `yam.yaml`. Допускается
до16 alias; target — строго `namespace.name`. Provider должен быть включён,
совместим с устройством и объявлять binary oneshot с `exec`. Его descriptor,
launcher и library paths проверяются в выбранной версии по тем же правилам.

Ответ содержит `plugin_id`, `version`, `directory`, `program`, `interpreter`
(строка `sh` или null), `args` и `library_path`. Все пути — pinned payload.
Host **не выполняет** команду и не выдаёт caller токен provider: caller с уже
разрешённым `exec` вызывает launcher сам, аргументами процесса, с собственным
таймаутом/лимитом вывода. API не раскрывает произвольный не объявленный пакет.
Резолвить перед каждым вызовом: отключение provider должно сразу запрещать
новые операции. Проверка firmware `check-write` остаётся в station-volume,
не в Luna. Для обновления YAM по-прежнему требуется остановить все leased
binary-плагины. Не кэшировать launcher между invocation/версиями.

Пакет `yam/luna` объявляет virtual capability `luna-binary-dependencies-v1`;
v2-пакет может требовать её в `depends`, чтобы один номер0.2.7 не был ошибочно
принят за наличие нового API. Registry должен публиковать соответствующий
`provides`; добавление поля в исходники не означает, что такой артефакт уже выпущен.

Пути runtime привязаны к установленному пакету:

```text
/api/runtime/:id/...
```

Каждый запрос должен содержать токен:

```text
X-Luna-Token: TOKEN
```

Доступ дополнительно проверяется permission-ами manifest пакета.

| Метод | Endpoint | Permission |
|---|---|---|
| `GET` | `/api/runtime/:id/device` | `exec` |
| `GET` | `/api/runtime/:id/sensors` | `sensors` |
| `GET` | `/api/runtime/:id/media` | `exec` |
| `POST` | `/api/runtime/:id/speech/next` | `ipc.speech` |
| `POST` | `/api/runtime/:id/command/cancel` | `commands` + `ipc.speech` |
| `POST` | `/api/runtime/:id/led` | `led` |
| `POST` | `/api/runtime/:id/led/ring` | `led` |
| `POST` | `/api/runtime/:id/led/logo` | `led` |
| `POST` | `/api/runtime/:id/led/off` | `led` |
| `POST` | `/api/runtime/:id/led/off/ring` | `led` |
| `POST` | `/api/runtime/:id/led/off/logo` | `led` |
| `POST` | `/api/runtime/:id/clock` | `clock` |
| `POST` | `/api/runtime/:id/clock/off` | `clock` |

Runtime LED и clock используют те же JSON-тела, что и основные endpoints.

`GET /media` возвращает доступные пакету возможности (`speech`, `commands`,
`led`) и `led_count`.

`POST /speech/next` возвращает следующее состояние Alice или `null` по
таймауту:

```json
{
  "phase": "recognition",
  "request_id": "request-id",
  "transcript": "громче",
  "final": true,
  "initial_snapshot": false
}
```

У каждого пакета свой reader; параллельный запрос к тому же reader отклоняется.
Для отмены объявленной команды передайте её `request_id`:

```sh
curl -X POST "$BASE/runtime/example.plugin/command/cancel" \
  -H 'X-Luna-Token: TOKEN' \
  -H 'Content-Type: application/json' \
  -d '{"request_id":"request-id"}'
```

Отмена разрешена только для `commands` + `ipc.speech`, только для финальной
фразы из `[commands].phrases` и в коротком окне после распознавания.

## Проверка возможностей из Lua

В Lua-плагине аппаратные зоны доступны через глобальную таблицу `device`.

```lua
if device.HasRing then
  alice.led.set.ring({"#ff0000"})
end
```

Для микрофона используйте `device.has_microphone` (или `device.HasMicrophone`).
Функция считается доступной только при наличии штатного `maind` и capability
`microphone.capture`.

Доступные поля LED:

```lua
device.HasRing       -- есть ли ring
device.RingCount     -- количество LED ring
device.HasLogo       -- есть ли logo
device.LogoCount     -- количество LED logo
```

Для Lua-кода также доступны snake_case-алиасы:

```lua
device.has_ring
device.ring_count
device.has_logo
device.logo_count
```

Capability-модули без отдельной topology-проверки имеют `is_available()`:

```lua
if device.has_clock then
  alice.clock.time("12:34", 170)
end

if alice.input.is_available() then
  -- работа с input API
end
```

Для LED проверяйте конкретную зону через `device`:

```lua
if device.has_ring then
  alice.led.set.ring({"#ff0000"})
end

if device.has_logo then
  alice.led.set.logo({"#0000ff"})
end

if device.has_ring then
  alice.led.off.ring()
end
```

Проверки `device.has_*` описывают topology станции. Permission из manifest
остаётся обязательным, например `led`, `clock`, `input` или `bluetooth`.

## Веб-панель

Если в настройках включены `web.ui` и `web.api`, UI доступен по корню:

```text
GET /
```

UI позволяет устанавливать `.yap` пакеты, управлять LED, часами и основными
настройками без ручных HTTP-запросов. В разделе «Тест API» можно одним запуском
проверить безопасные endpoints с логом результатов; операции, которые ждут
физическое действие или меняют состояние станции, отмечаются как пропущенные.

### Своя вкладка из Lua-плагина

Плагин с разрешением `web` может зарегистрировать HTML-файл:

```lua
local url = register.customweb("web/index.html")
```

После регистрации страница доступна по `GET /web/<plugin-id>` и автоматически
появляется отдельной вкладкой в веб-панели Luna. Список вкладок можно получить
через `GET /api/custom-web`. Файл должен быть относительным `.html`/`.htm`
внутри каталога плагина и не превышать 4 МБ. Luna не выдаёт произвольные файлы
пакета: наружу публикуется только явно зарегистрированный файл. Регистрация
удаляется при остановке плагина.

## Примечания по безопасности

- Для локального тестирования используйте `--listen 127.0.0.1:8100`.
- `0.0.0.0` открывает UI и API всем узлам доступной сети.
- Плагины и runtime-пакеты получают доступ только к API, разрешённому их
  manifest permissions.
- Автообновление Luna из GitHub по умолчанию отключено; его можно включить в
  `settings.toml` или через `/api/settings`.

## Завершение пакетов

Stop, disable, remove и повторный запуск ждут завершения прежнего процесса.
Сначала SIGTERM получает главный процесс пакета: у него есть 15 секунд на
очистку с ещё работающими помощниками. Затем оставшаяся группа получает SIGTERM,
через 2 секунды — SIGKILL. При SIGTERM/SIGINT Luna запрещает новые запуски,
возвращает временную индикацию и ждёт очистки пакетов до 20 секунд.

Пакет должен оставаться в foreground и обрабатывать SIGTERM. Процессы,
самостоятельно покинувшие группу, этим контрактом не охватываются. Принудительное
завершение не гарантирует восстановления аппаратного состояния. Установка
архива поверх работающего пакета не входит в этот механизм: сначала остановите
пакет.
