# Luna

Luna — это фреймворк и runtime для пакетов, которые должны работать на
разных станциях Яндекса.

Пакет не общается с железом напрямую. Он обращается к Luna API, а Luna уже
выбирает подходящую реализацию для конкретной станции: sysfs, I²C, evdev,
framebuffer, ALSA или штатный IPC Quasar. Поэтому один пакет может работать на
Mini, Mini 3, Midi, Micro и других устройствах без копии кода для каждой
модели.

```text
Lua-пакет
    ↓  Luna API и permissions
Luna runtime / gateway
    ↓  общий контракт capability
station crate конкретной модели
    ↓
железо станции или штатный сервис Quasar
```

Главная идея Luna — отделить логику пакета от особенностей устройства. Автор
пакета проверяет, доступна ли нужная возможность, и вызывает единый API:

```lua
if device.has_ring then
  alice.led.set.ring({"#00aaff"})
end

if device.has_clock then
  alice.clock.time("12:34", 170)
end
```

Если capability нет, пакет должен пропустить эту часть интерфейса или выбрать
запасной вариант. Ему не нужно знать, сколько LED находится в кольце другой
модели и какой sysfs-путь использует станция.

## Документация

- [Как написать пакет на Luna](docs/PACKAGE_GUIDE.md)
- [Как добавить поддержку станции](docs/DEVICE_GUIDE.md)
- [Полный HTTP API](docs/API.md)
- [Lua-плагины через YAM](docs/PLUGINS.md)
- [Медиапровайдеры](docs/MEDIA_PROVIDERS.md)
- [Manifest поддерживаемых станций](luna-manifest.json)

## Установка

### Через YAM

Если YAM уже установлен на станции, добавьте официальный реестр и установите
runtime:

```sh
/data/yam/bin/yam-run registry add official \
  https://repo.yse.wtf/v2 \
  --key ed25519:5Rz8F47uftWVU5kLgWYhprkEZ4rpP2kecUge/a7KQas=
/data/yam/bin/yam-run install yam/luna
```

YAM выбирает бинарь по платформе станции. Если автоматическое определение не
сработало, платформу можно указать явно:

```sh
YAM_PLATFORM=yandexmini /data/yam/bin/yam-run install yam/luna
```

Через YAM устанавливается пакет, а запускает его сервис YAM. На платформах без
автоматического boot hook сервис нужно запустить способом, предусмотренным
конфигурацией конкретной станции.

### Установка с компьютера

Для macOS и Linux нужны `adb`, подключённая станция и включённая ADB-отладка:

```sh
curl -fsSL https://raw.githubusercontent.com/nyakokitsu/luna/main/install.sh | sh
```

Если станций несколько:

```sh
ADB_SERIAL=<serial> sh install.sh
```

Для Windows PowerShell:

```powershell
Set-ExecutionPolicy -Scope Process Bypass
irm https://raw.githubusercontent.com/nyakokitsu/luna/main/install.ps1 -OutFile install.ps1
.\install.ps1
```

Установщик получает имя станции через `uname -n`, находит его среди `models` в
релизном manifest и скачивает подходящий бинарь. Удаление в PowerShell:

```powershell
.\install.ps1 -Uninstall
```

### Два способа установки

YAM и ручная установка могут существовать на одной станции, но одновременно
работает только один экземпляр Luna. Все актуальные бинарники используют
общий `/tmp/luna.lock`. Новый экземпляр завершает уже запущенный Luna,
независимо от того, из какой папки он был запущен.

## Что получает пакет

- один Lua API для LED, часов, громкости, input, датчиков, аудио, Bluetooth и
  локальных команд;
- `device.has_*`, topology и `is_available()` для проверки возможностей;
- permissions для ограничения доступа пакета;
- HTTP API для управления и диагностики;
- веб-панель с ручными тестами API;
- собственные вкладки Web UI через `register.customweb()`;
- единый способ запускать локальные и установленные через YAM пакеты;
- аппаратную поддержку в отдельных station crate, а не в общем gateway.

Пакет не должен разбирать модель станции, открывать I²C/ALSA/evdev или писать
в sysfs. Если для работы нужна новая возможность, её сначала добавляют в crate
станции и общий capability-контракт Luna.

## Поддерживаемые станции

| Device | Codename | Target |
| --- | --- | --- |
| `yandexmini3` | `bergamot` | `aarch64-unknown-linux-musl` |
| `yandexmini` | `yandexmini` | `aarch64-unknown-linux-musl` |
| `yandexmicro` | `yandexmicro` | `aarch64-unknown-linux-musl` |
| `yandexmini_2` | `yandexmini_2` | `aarch64-unknown-linux-musl` |
| `yandexmidi` | `cucumber` | `aarch64-unknown-linux-musl` |
| `yandexplum` | `plum` | `aarch64-unknown-linux-musl` |

Полный список моделей и соответствий находится в
[`luna-manifest.json`](luna-manifest.json).

## Разработка Luna

Проверка workspace:

```sh
cargo test --workspace --locked
```

Сборка и установка выбранной станции на подключённое устройство:

```sh
tools/build-deploy.sh --device yandexmini3
```

Новая станция получает собственную папку `stations/<device>/` со своим
`station_main.rs`, hardware backend и capability-функциями. Общий gateway не
должен содержать модельные имена, числа LED, sysfs-пути или I²C-адреса.
