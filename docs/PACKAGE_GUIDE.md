# Как написать пакет для Luna

Luna-пакет — приложение, которое запускается на станции и работает с её
возможностями через единый API. Luna уже установлен как runtime, поэтому
пакет не содержит драйверы и не обращается к sysfs, I²C, ALSA или evdev
напрямую.

```text
пакет → Luna API → capability → station crate → железо / Quasar
```

Такой подход позволяет использовать один Lua-код на Mini, Mini 3, Midi, Micro
и других станциях. Различия оборудования остаются внутри station crate.

## Структура

```text
my-plugin/
├── yam.yaml
└── plugin/
    ├── plugin.toml
    ├── main.lua
    └── web/index.html       # необязательно
```

`yam.yaml` описывает установку файлов:

```yaml
name: vasya/clock
version: 1.0.0
license: MIT
description: Часы для Luna
targets: [any]
depends: [yam/luna ^0.2]
files:
  share/luna/plugins/vasya.clock/: plugin/
```

`targets: [any]` подходит для Lua-пакета без native-бинарей. Для пакета с
нативным кодом нужны отдельные targets и артефакты под платформы. Формат
упаковки описан в [документации YAM](https://github.com/nyakokitsu/yam/blob/main/docs/PACKAGING.md).

## Manifest Luna

```toml
id = "vasya.clock"
name = "Часы"
version = "1.0.0"
entry = "main.lua"
description = "Показывает время на доступном дисплее"
permissions = ["clock", "led"]

[compatibility]
features = ["clock.display"]
```

`id` должен совпадать с каталогом `namespace.name`. `entry` указывается
относительно каталога плагина. В `permissions` перечисляйте только нужные
возможности. Не добавляйте сюда sysfs-пути, I²C-адреса и имена устройств — это
детали реализации станции.

## Код пакета

Проверяйте capability перед использованием:

```lua
if device.has_ring then
  alice.led.set.ring({"#00aaff"})
end

if device.has_logo then
  alice.led.set.logo({"#ffffff"})
end

if device.has_clock then
  alice.clock.time("12:34", 170)
end
```

Полезные поля `device`:

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

Для возможностей без отдельного поля используйте `is_available()`:

```lua
if alice.input.is_available() then
  local event = alice.input.next_event(1000)
end

if alice.audio.is_available() then
  alice.audio.play("sounds/notification.wav")
end
```

PascalCase-имена вроде `device.HasRing` оставлены для совместимости. В новом
коде используйте snake_case.

## LED и цвета

```lua
if device.has_ring then
  alice.led.set.ring({"#ff0000", "#0000ff"})
  alice.led.off.ring()
end

if device.has_logo then
  alice.led.set.logo({"#ffffff"})
  alice.led.off.logo()
end
```

Список цветов описывает всю зону. Luna распределяет его по физическим LED:
один цвет заполняет зону, два делят её пополам, три — на три части. Если
деление не получается ровным, разница между частями составляет не больше
одного LED.

Количество цветов не обязано совпадать с количеством LED. Если цветов больше,
чем LED в выбранной зоне, API возвращает `400`, а не молча отбрасывает данные.
Пакет может заранее ограничить список по `device.ring_count` или
`device.logo_count`.

## Звук, громкость и input

Для WAV-файлов нужен permission `audio`:

```toml
permissions = ["audio"]
```

```lua
if alice.audio.is_available() then
  alice.audio.play("sounds/notification.wav")
end

if alice.volume.is_available() then
  alice.volume.set(50)
end
```

На станциях с Quasar Luna использует штатный звуковой маршрут. Не запускайте
`aplay`, не подменяйте ALSA и не поднимайте второй аудиодемон: это может
сломать Алису, Bluetooth и регулировку громкости.

Для кнопок и жестов используйте семантические действия (`volume_up`, `activate`,
`mute`), если они доступны, а не коды Linux-клавиш. Команды Алисы и речь также
возвращают промежуточные и финальные результаты — первую расшифровку нельзя
считать окончательной без проверки поля `final`.

## Своя вкладка в веб-панели

```toml
permissions = ["web"]

[launch]
mode = "service"
```

```lua
local url = register.customweb("web/index.html")
print(url) -- /web/vasya.clock

while true do
  alice.sleep(60000)
end
```

Страница появляется отдельной вкладкой и может обращаться к API через
`fetch('/api/...')`. Разрешены только HTML-файлы из каталога пакета размером
до 4 МБ. Регистрация живёт, пока работает плагин.

## Разрешения

Чаще всего используются `led`, `clock`, `audio`, `input`, `microphone`,
`bluetooth`, `web`, `exec` и `network`. Запрашивайте только необходимые
разрешения. Permission не означает, что capability есть на каждой станции:
доступность всё равно проверяется через `device.has_*` или `is_available()`.

## Проверка и установка

```sh
yam-publish pack .
yam install vasya/clock
```

Проверьте пакет хотя бы на двух станциях с разным железом и на устройстве без
опциональных capability. Пакет не должен падать из-за отсутствующего ring,
logo, clock, микрофона или штатного сервиса.

Файлы плагина устанавливает и удаляет YAM. Luna только читает их, запускает
код и предоставляет API.
