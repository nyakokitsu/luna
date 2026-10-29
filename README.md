# Luna

> Универсальный Lua runtime и HTTP-шлюз для станций Яндекса.

Luna связывает Lua-плагины и HTTP API с реальным железом станции. Аппаратная
логика не зашита в общий gateway: каждая модель собирается отдельным crate и
сама описывает свои LED, кнопки, часы, аудио и другие capabilities.

[API reference](docs/API.md) · [Lua/YAM plugins](docs/PLUGINS.md) · [Manifest](luna-manifest.json)

## Быстрая установка

Установщики скачивают подходящий asset из GitHub Release, сверяют SHA-256,
определяют модель и архитектуру станции, устанавливают Luna в
`/data/quazar/luna` и создают автозапуск `/etc/init.d/S99luna`.

Требования: Android Platform Tools с `adb`, подключённая станция и включённая
ADB-отладка.

### macOS / Linux

```sh
curl -fsSL https://raw.githubusercontent.com/nyakokitsu/luna/main/install.sh | sh
```

Для конкретной станции или версии:

```sh
ADB_SERIAL=<serial> sh install.sh
sh install.sh v0.2.0
```

При нескольких устройствах установщик предложит интерактивный выбор.

### Windows PowerShell

WSL, Python и YAM не нужны:

```powershell
Set-ExecutionPolicy -Scope Process Bypass
irm https://raw.githubusercontent.com/nyakokitsu/luna/main/install.ps1 -OutFile install.ps1
.\install.ps1
```

Дополнительные параметры:

```powershell
.\install.ps1 -Serial <serial>
.\install.ps1 -Version v0.2.0
.\install.ps1 -Serial <serial> -Uninstall
```

### Установка через YAM

Если на станции уже установлен YAM и настроен registry, Luna можно поставить
как обычный пакет. YAM сам выберет опубликованный артефакт под платформу:

```sh
yam --platform yandexmicro install yam/luna
```

Для других станций используйте соответствующий platform ID: `yandexmini`,
`yandexmini_2`, `yandexmini3`, `yandexmidi` или `yandexplum`.

Проверить план установки без изменений:

```sh
yam --platform yandexmicro plan yam/luna
```

Standalone-установщик и пакет YAM — два независимых способа установки. Не
запускайте их одновременно на одном `--data-dir`.

## Возможности

- отдельный бинарник под каждую поддерживаемую станцию;
- общий HTTP API и встроенная веб-панель;
- Lua runtime для пакетов и пользовательских скриптов;
- permissions для Lua: LED, input, network, audio, system и другие;
- LED ring/logo с равномерным распределением цветов;
- кнопки и распознанные жесты Mini 3;
- сегментные часы, датчики, громкость и системные capabilities;
- локальная метка комнаты для API, веб-панели и Lua;
- установка напрямую через ADB, без YAM.

## Архитектура

```text
HTTP / Web UI
      │
      ▼
src/main.rs → src/gateway.rs → station crate
                              ├── device.rs
                              ├── station_main.rs
                              └── hardware.rs / capabilities
```

Каждая станция живёт в собственной папке `stations/<device>/`. Общий gateway
не содержит таблиц аппаратных параметров. Модели, codename, Rust target и
release asset описаны в [`luna-manifest.json`](luna-manifest.json).

## Поддерживаемые станции

| Модель | Codename | Бинарник | Target |
| --- | --- | --- | --- |
| `yandexmini3` | `bergamot` | `luna-yandexmini3` | `aarch64-unknown-linux-musl` |
| `yandexmini` | `mango` | `luna-yandexmini` | `armv7-unknown-linux-musleabihf` |
| `yandexmicro` | `yandexmicro` | `luna-yandexmicro` | `aarch64-unknown-linux-musl` |
| `yandexmini_2` | `yandexmini_2` | `luna-yandexmini_2` | `aarch64-unknown-linux-musl` |
| `yandexmidi` | `cucumber` | `luna-yandexmidi` | `aarch64-unknown-linux-musl` |
| `yandexplum` | `plum` | `luna-yandexplum` | `aarch64-unknown-linux-musl` |

Неизвестная модель считается неподдерживаемой. Для добавления станции нужны
новый crate, hardware/capabilities и запись в manifest.

## API

После установки API доступен на порту `8099`:

```sh
BASE=http://<IP Станции>:8099/api
curl "$BASE/device"
```

Основные группы:

| Группа | Назначение |
| --- | --- |
| `/device` | модель, архитектура, topology и capabilities |
| `/led` | ring/logo, цвета и выключение |
| `/clock` | сегментные часы |
| `/input` | input devices, кнопки и жесты |
| `/settings` | настройки Luna и permissions |
| `/lua` | пользовательские Lua-файлы |
| `/plugins` | управление плагинами |
| `/runtime` | состояние runtime |

Пример LED:

```sh
curl -X POST "$BASE/led/ring" \
  -H 'Content-Type: application/json' \
  -d '{"colors":["#FF0000","#0000FF"]}'
```

Успешные синхронные операции возвращают `200 OK`. Асинхронные операции
возвращают `202 Accepted`, ошибки — HTTP-код и JSON:

```json
{"error":"описание ошибки"}
```

Возможность конкретной функции сначала проверяйте через capabilities в
`GET /api/device`. В Lua используйте `.is_available()`; для LED доступны
`device.has_ring`, `device.ring_count`, `device.has_logo` и
`device.logo_count`.

Полное описание endpoint-ов и примеры запросов: [docs/API.md](docs/API.md).

Локальную комнату станции можно задать через `PUT /api/room` или командой
`luna --data-dir <data-dir> room "Кухня"`. Она хранится отдельно от профиля
устройства и доступна в Lua как `device.room`.

## Пользовательский Lua

Скрипты можно хранить в `share/lua` и запускать без установки пакета.
Функция отключена по умолчанию и включается через `lua.enabled`. Для каждого
скрипта действуют отдельные permissions и ограничение времени выполнения.

Особенно опасные возможности — бесконечные циклы, сеть, `exec`, IPC и
команды — выключены по умолчанию. Настройки меняются через
`PUT /api/settings` или веб-панель.

## Сборка

Host-сборка:

```sh
cargo build --release --locked
```

Сборка всех станций:

```sh
tools/build-stations.sh
```

Сборка одной станции:

```sh
tools/build.sh --device yandexmidi
```

Или запустите `tools/build.sh` без параметров для выбора стрелками. Для
кросс-сборки на macOS установите targets:

```sh
rustup target add aarch64-unknown-linux-musl
rustup target add armv7-unknown-linux-musleabihf
```

Результат появляется в `dist/`: бинарники находятся в `dist/assets/`,
описание assets — в `dist/manifest.json`, checksums — в
`dist/SHA256SUMS`.

Проверка workspace:

```sh
cargo test --workspace --locked
```

Изолированная проверка media/speech моста (python3 и собранный бинарник;
колонка, звук и LED не используются):

```sh
python3 tests/media_api.py target/debug/luna
```

## Разработка станции

1. Создайте `stations/<device>/`.
2. Добавьте `Cargo.toml`, `station_main.rs`, hardware и capability-модули.
3. Зарегистрируйте crate в workspace.
4. Добавьте модель, codename, target и asset в `luna-manifest.json`.
5. Соберите конкретную станцию и проверьте её API.

Локальные `tools/release.sh` и `tools/build-deploy.sh` используются для
публикации и deploy и не входят в Git. Условия лицензии будут добавлены позже.
