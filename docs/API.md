# Luna HTTP API

Документация для встроенного HTTP API Luna.

По умолчанию Luna слушает `127.0.0.1:8099`. Для доступа по сети нужно явно
запустить её с адресом вроде `--listen 0.0.0.0:8100`.

Все примеры ниже используют:

```sh
BASE=http://<IP Станции>:8099/api
```

Успешные синхронные операции возвращают `200 OK`. Запуск пакета, который
продолжается в фоне, возвращает `202 Accepted`.
JSON-операции требуют заголовок:

```text
Content-Type: application/json
```

При ошибке API возвращает `400 Bad Request` и JSON:

```json
{"error":"описание ошибки"}
```

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

## User Lua без установки пакета

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

### Разрешения user Lua

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

Цвета передаются компактным списком. Luna сама равномерно расширяет его по
физическому числу LED станции.

### Установка цветов

| Метод | Endpoint | Действие |
|---|---|---|
| `POST` | `/api/led` | Ring и logo, если logo есть |
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

Возвращает доступность часов и список допустимых сегментов.

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

### `POST /api/clock/off`

Выключает управление часами без тела запроса:

```sh
curl -X POST "$BASE/clock/off"
```

## Input и кнопки

### `GET /api/input/devices`

Список доступных evdev-устройств:

```sh
curl "$BASE/input/devices"
```

### Semantic actions Mini 3

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

### Raw input

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

Пути runtime привязаны к установленному пакету:

```text
/api/runtime/:id/...
```

Каждый запрос должен содержать токен:

```text
X-Luna-Token: TOKEN
```

Для уже установленных legacy-пакетов принимается и старый заголовок
`X-YAM-Token` с тем же значением; если переданы оба заголовка с разными
значениями, запрос отклоняется.

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

`/media` возвращает доступные пакету возможности (`speech`, `commands`, `led`)
и `led_count`. `/speech/next` отдаёт следующее состояние Alice (`phase`,
`request_id`, `transcript`, `final`, `initial_snapshot`) или `null` по
таймауту; параллельный опрос того же пакета отклоняется. `/command/cancel`
отменяет ответ Alice только для финальной фразы, объявленной в
`[commands].phrases`, в пределах короткого окна после распознавания.

## Lua capability checks

В Lua-плагине аппаратные зоны доступны через глобальную таблицу `device`.

```lua
if device.HasRing then
  alice.led.set.ring({"#ff0000"})
end
```

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

Capability-модули имеют собственную проверку доступности:

```lua
if alice.led.is_available() then
  alice.led.set({"#00ff00"})
end

if alice.clock.is_available() then
  alice.clock.time("12:34", 170)
end

if alice.input.is_available() then
  -- работа с input API
end
```

Для callable LED API проверка находится прямо на вызываемом объекте:

```lua
if alice.led.set.ring.is_available() then
  alice.led.set.ring({"#ff0000"})
end

if alice.led.set.logo.is_available() then
  alice.led.set.logo({"#0000ff"})
end

if alice.led.off.ring.is_available() then
  alice.led.off.ring()
end
```

`is_available()` проверяет аппаратную capability станции. Она не заменяет
permission из manifest: плагин всё равно должен запросить соответствующее
разрешение, например `led`, `clock`, `input` или `bluetooth`.

## Web UI

Если в настройках включены `web.ui` и `web.api`, UI доступен по корню:

```text
GET /
```

UI позволяет устанавливать `.yap` пакеты, управлять LED, часами и основными
настройками без ручных HTTP-запросов.

## Примечания по безопасности

- Для локального тестирования используйте `--listen 127.0.0.1:8100`.
- `0.0.0.0` открывает UI и API всем узлам доступной сети.
- Плагины и runtime-пакеты получают доступ только к API, разрешённому их
  manifest permissions.
- Автообновление Luna из GitHub по умолчанию отключено; его можно включить в
  `settings.toml` или через `/api/settings`.
