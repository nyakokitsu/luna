# Luna

Luna — runtime и API-слой для пакетов, которым нужно работать с разными
моделями станций Яндекса.

Главная задача Luna — дать пакету единый API, скрывающий различия железа.
Пакет работает с `Luna API`, а station crate конкретной модели переводит эти
операции в sysfs, I²C, ALSA, evdev и другие локальные интерфейсы устройства.

```text
Lua-пакет
   │  Luna API / permissions
   ▼
Luna gateway
   │  единый hardware contract
   ▼
station crate → конкретное железо станции
```

## Документация

- [Гайд: как строить пакет на Luna](docs/PACKAGE_GUIDE.md)
- [Гайд: как добавить новую станцию](docs/DEVICE_GUIDE.md)
- [Полный HTTP API](docs/API.md)
- [Плагины через YAM](docs/PLUGINS.md)
- [Manifest поддерживаемых станций](luna-manifest.json)

## Установка

### Через YAM

Рекомендуемый способ, если Yam уже установлен на станции:

```sh
yam --platform yandexmini3 install yam/luna
```

YAM выбирает пакет под платформу станции. Для других устройств используйте
соответствующий platform ID из [`luna-manifest.json`](luna-manifest.json).

Standalone-установщик и установка через YAM — альтернативные способы; не
используйте их одновременно с одним и тем же `data-dir`.

### macOS / Linux

Нужны `adb`, подключённая станция и включённая ADB-отладка:

```sh
curl -fsSL https://raw.githubusercontent.com/nyakokitsu/luna/main/install.sh | sh
```

Для нескольких станций установщик предложит выбор. Для конкретной станции:

```sh
ADB_SERIAL=<serial> sh install.sh
```

### Windows PowerShell

```powershell
Set-ExecutionPolicy -Scope Process Bypass
irm https://raw.githubusercontent.com/nyakokitsu/luna/main/install.ps1 -OutFile install.ps1
.\install.ps1
```

Установщик сам получает hostname через `uname -n`, находит его в `models`
релизного manifest и скачивает соответствующий бинарь.

Удаление:

```powershell
.\install.ps1 -Uninstall
```

## Что даёт Luna пакету

- единый Lua API для LED, часов, громкости, input, сенсоров, аудио, Bluetooth и IPC;
- `is_available()` и device topology для безопасной работы на разных моделях;
- permissions и ограничения сети, input, IPC, команд и системных операций;
- единый HTTP API для управления и диагностики;
- встроенная веб-панель с автоматическими тестами API;
- вкладки Custom Web для интерфейсов Lua-пакетов;
- запуск Lua-пакетов через локальную установку или YAM;
- отдельные station crates без хардкода аппаратных параметров в gateway.

Пакет не должен определять модель по sysfs-путям и не должен напрямую открывать
I²C/ALSA/evdev. Он проверяет capability и вызывает Luna API.

### Интерфейсы пакетов в веб-панели

Lua-пакет может добавить собственную вкладку в веб-панель Luna. Для этого ему
нужно разрешение `web` и работа в режиме сервиса:

```toml
permissions = ["web"]

[launch]
mode = "service"
```

```lua
register.customweb("web/index.html")
```

После регистрации страница появляется отдельной вкладкой рядом с вкладкой
Luna. Она открывается по `/web/<plugin-id>` и может вызывать API Luna через
`fetch('/api/...')`. Публикуются только явно зарегистрированные HTML-файлы из
каталога пакета; вкладка удаляется после остановки плагина.

## Поддерживаемые станции

| Device | Codename | Target |
| --- | --- | --- |
| `yandexmini3` | `bergamot` | `aarch64-unknown-linux-musl` |
| `yandexmini` | `yandexmini` | `aarch64-unknown-linux-musl` |
| `yandexmicro` | `yandexmicro` | `aarch64-unknown-linux-musl` |
| `yandexmini_2` | `yandexmini_2` | `aarch64-unknown-linux-musl` |
| `yandexmidi` | `cucumber` | `aarch64-unknown-linux-musl` |
| `yandexplum` | `plum` | `aarch64-unknown-linux-musl` |

## Разработка Luna

```sh
cargo test --workspace --locked
```

Для сборки и установки на подключённую станцию используйте единственный
публичный helper:

```sh
tools/build-deploy.sh --device yandexmini3
```

Для новой станции создаётся `stations/<device>/` со своими `device.rs`,
`station_main.rs` и hardware/capability-модулями. Общий gateway должен
оставаться независимым от конкретных sysfs-путей и I²C-адресов.
