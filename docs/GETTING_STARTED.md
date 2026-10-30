# Начало работы с Luna

Этот документ — короткий путь от установленной Luna до первого работающего
пакета. Если нужен полный список HTTP-методов, смотрите [API.md](API.md), а
формат station crate — в [DEVICE_GUIDE.md](DEVICE_GUIDE.md).

## Что такое Luna

Luna — runtime и общий API для пакетов, которые должны работать на разных
станциях. Пакет не знает, где лежит LED, как называется input-устройство и
какой аудиосервис запущен. Он вызывает Luna API, а station crate выбирает
реализацию для конкретного железа.

```text
Lua-пакет → Luna API → capability → station crate → железо или Quasar
```

YAM в этой схеме отвечает за установку, обновление и удаление файлов пакета.
Luna загружает пакет и предоставляет ему API. Это два разных слоя: пакет не
должен запускать или устанавливать второй runtime самостоятельно.

## 1. Установить Luna

### Через YAM

На станции с установленным YAM:

```sh
/data/yam/bin/yam-run install yam/luna
```

YAM сам выбирает артефакт для платформы станции. Если пакет ставится вручную,
убедитесь, что бинарь собран под правильный `device`, архитектуру и libc.

### Для разработки с компьютера

Нужны `adb`, доступ к станции и включённая ADB-отладка. После установки
проверьте, что Luna отвечает:

```sh
curl http://<IP-станции>:<порт>/api/device
```

В ответе будут модель, архитектура и список доступных функций. Порт зависит от
запуска Luna; в примерах проекта используется `8099` или `8100`.

## 2. Создать пакет

Минимальная структура:

```text
my-plugin/
├── yam.yaml
└── plugin/
    ├── plugin.toml
    └── main.lua
```

`yam.yaml` описывает пакет для YAM:

```yaml
name: vasya/hello-luna
version: 0.1.0
license: MIT
description: Первый пакет для Luna
targets: [any]
depends: [yam/luna ^0.2]
files:
  share/luna/plugins/vasya.hello-luna/: plugin/
```

`targets: [any]` подходит для чистого Lua. Если в пакете есть native-бинарь,
нужны отдельные targets и сборка под каждую поддерживаемую платформу.

`plugin/plugin.toml`:

```toml
id = "vasya.hello-luna"
name = "Hello Luna"
version = "0.1.0"
entry = "main.lua"
description = "Проверяет доступные возможности станции"
permissions = ["led"]
```

`id` должен совпадать с каталогом `namespace.name` в `yam.yaml`.

## 3. Написать первый `main.lua`

```lua
print("Luna plugin started")

if device.has_ring then
  alice.led.set.ring({"#00aaff"})
elseif device.has_logo then
  alice.led.set.logo({"#00aaff"})
end
```

Проверяйте наличие функции до вызова. Например:

```lua
if device.has_clock then
  alice.clock.time("12:34", 170)
end

if alice.audio.is_available() then
  alice.audio.play("sounds/notification.wav")
end

if alice.input.is_available() then
  local event = alice.input.next_event(1000)
end
```

`permissions` и доступность capability — разные вещи. Permission разрешает
пакету запросить функцию, но не гарантирует, что конкретная станция умеет её
выполнять.

Основные поля `device`:

```lua
device.model
device.architecture
device.features
device.has_ring
device.ring_count
device.has_logo
device.logo_count
device.has_clock
device.has_microphone
device.has_mute_button
device.room
```

Не сравнивайте модель станции в обычном пакете и не встраивайте sysfs-пути.
Если capability отсутствует, выберите запасной вариант или спокойно пропустите
операцию.

## 4. Важные правила API

Luna сама распределяет цвета по LED выбранной зоны. Пакет может знать количество
LED через `device.ring_count` или `device.logo_count`, но не должен повторять
аппаратную раскладку. Если цветов меньше, чем LED, они равномерно занимают всю
зону. Если цветов больше, чем LED, API возвращает ошибку — лишние значения не
отбрасываются молча.

Для звука используйте `alice.audio.play()` и не запускайте свой `aplay`, ALSA
или второй аудиодемон. Для громкости используйте `alice.volume`, а для кнопок и
жестов — семантические действия Luna, если они доступны.

## 5. Проверить пакет

Сначала проверьте Lua-код на станции вручную, затем установите пакет через YAM.
После установки:

```sh
curl http://<IP-станции>:<порт>/api/device
curl http://<IP-станции>:<порт>/api/plugins
```

В веб-панели Luna можно:

- посмотреть модель и capabilities;
- вызвать endpoint вручную;
- проверить LED, часы, звук и input;
- открыть вкладку плагина, если он зарегистрировал custom web;
- посмотреть ответ и ошибку каждого запроса.

Проверяйте пакет минимум на станции с нужной capability и на станции без неё.
Отсутствующий ring, clock, микрофон или Bluetooth не должны приводить к падению
пакета.

## 6. Собственная вкладка в веб-панели

Добавьте permission и зарегистрируйте HTML-файл:

```toml
permissions = ["web"]

[launch]
mode = "service"
```

```lua
local url = register.customweb("web/index.html")
print(url)

while true do
  alice.sleep(60000)
end
```

Страница появится отдельной вкладкой в Luna Web UI и сможет обращаться к
`/api/...` через обычный `fetch`.

## 7. Отладка

Если capability не работает:

1. вызовите `GET /api/device` и проверьте `features`;
2. проверьте соответствующий `device.has_*` или `is_available()`;
3. вызовите endpoint через веб-панель и сохраните JSON-ошибку;
4. проверьте permission в `plugin.toml`;
5. сравните поведение на другой станции.

Если проблема зависит от модели или железа, исправление нужно делать в
`stations/<device>/`, а не добавлять условие в Lua-пакет или общий gateway.

## Дальше

- [Как написать пакет](PACKAGE_GUIDE.md) — полный Lua API и манифест.
- [Lua-плагины через YAM](PLUGINS.md) — расположение файлов и жизненный цикл.
- [HTTP API](API.md) — все endpoint’ы.
- [Как добавить станцию](DEVICE_GUIDE.md) — station crate и hardware backend.
- [Медиапровайдеры](MEDIA_PROVIDERS.md) — отдельный контракт для плееров.
