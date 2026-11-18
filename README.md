# Luna

**Runtime и единый API для модов на станциях Яндекса.** Один пакет работает на Mini, Mini 2, Mini 3, Midi, Micro, Плюм и других станциях — без отдельной сборки под каждую модель.

[![CI](https://github.com/nyakokitsu/luna/actions/workflows/ci.yml/badge.svg)](https://github.com/nyakokitsu/luna/actions/workflows/ci.yml) [![Release](https://img.shields.io/github/v/release/nyakokitsu/luna?label=release)](https://github.com/nyakokitsu/luna/releases) [![Devices](https://img.shields.io/badge/devices-10-c4623f)](luna-manifest.json) [![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE) [![Rust](https://img.shields.io/badge/rust-2021-dea584?logo=rust&logoColor=white)](Cargo.toml)

[Документация](#документация) · [Установка](#установка) · [API](docs/API.md) · [Плагины](docs/PLUGINS.md) · [Станции](#станции)

---

## Что такое Luna

Мод не работает с железом напрямую. Он вызывает Luna API, а Luna выбирает реализацию под конкретную станцию: sysfs, I²C, evdev, framebuffer, ALSA или штатный IPC Quasar.

```lua
if device.has_ring then
  alice.led.set.ring({"#00aaff"})
end

if device.has_clock then
  alice.clock.time("12:34", 170)
end
```

Мод не знает, сколько LED в кольце другой модели, как называется input-устройство и какой аудиосервис запущен на прошивке. Если возможности нет, мод пропускает этот блок и работает дальше.

## Возможности

| Возможность | Что даёт |
| :-- | :-- |
| ![Единый Lua API](assets/icons/lua-api.svg) **Единый Lua API** | LED, часы, громкость, input, датчики, аудио, микрофон, Bluetooth, речь и команды Алисы |
| ![Проверка capability](assets/icons/capability.svg) **Проверка capability** | `device.has_*`, topology и `is_available()` — один код на все модели |
| ![Разрешения](assets/icons/permissions.svg) **Разрешения** | мод получает только то, что запросил в manifest |
| ![HTTP API](assets/icons/http-api.svg) **HTTP API** | управление станцией, диагностика и внешние HTTP(S)-запросы |
| ![Веб-панель](assets/icons/web-panel.svg) **Веб-панель** | тесты, настройки и LED-визуализатор прямо на станции |
| ![Общий стиль](assets/icons/panel-style.svg) **Общий стиль** | сторонние панели подключают `/luna-panel.css` и выглядят как часть Luna |
| ![YAM](assets/icons/yam-packages.svg) **YAM** | установка, обновление и удаление пакетов |
| ![Отдельные станции](assets/icons/station.svg) **Отдельные станции** | поддержка железа живёт в своём crate, а не в общем gateway |

## Архитектура

```text
Lua-пакет
    │  Luna API и permissions
    ▼
Luna runtime / gateway ──────────────► Web UI, HTTP API, user Lua
    │  общий контракт capability
    ▼
station crate конкретной модели
    │
    ▼
железо станции или штатный сервис Quasar
```

Два правила:

- мод не трогает железо: никакого sysfs, I²C, ALSA, evdev и собственных демонов;
- gateway не знает моделей: ни имён станций, ни числа LED, ни путей sysfs, ни адресов I²C — всё это в `stations/<device>/`.

## Установка

### Через YAM

На станции с установленным YAM:

```sh
/data/yam/bin/yam-run registry add official \
  https://repo.yse.wtf/v2 \
  --key ed25519:5Rz8F47uftWVU5kLgWYhprkEZ4rpP2kecUge/a7KQas=
/data/yam/bin/yam-run install yam/luna
```

YAM сам выбирает бинарь под платформу станции. Если автодетект не сработал, платформу можно указать явно:

```sh
YAM_PLATFORM=yandexmini /data/yam/bin/yam-run install yam/luna
```

Пакет ставит YAM, а запускает его сервис YAM. На платформах без автоматического boot hook сервис запускают так, как принято в конфигурации станции.

### С компьютера

Нужны `adb`, подключённая станция и включённая ADB-отладка.

```sh
curl -fsSL https://raw.githubusercontent.com/nyakokitsu/luna/main/install.sh | sh
```

Если станций несколько, серийный номер передаётся переменной:

```sh
curl -fsSL https://raw.githubusercontent.com/nyakokitsu/luna/main/install.sh | ADB_SERIAL=<serial> sh
```

Флаги установщика: `--platform`, `--listen`, `--no-boot-hook`, `--uninstall`,
`--telemetry`, `--no-telemetry`, `--quiet`. Полный список — `sh install.sh --help`.

Windows:

```powershell
Set-ExecutionPolicy -Scope Process Bypass
irm https://raw.githubusercontent.com/nyakokitsu/luna/main/install.ps1 -OutFile install.ps1
.\install.ps1
```

Удаление: `.\install.ps1 -Uninstall`.

### Из исходников

```sh
cargo build --workspace --locked
tools/build-deploy.sh --device yandexmini3
```

### YAM и ручная установка

Оба способа могут стоять на одной станции, но одновременно работает только один экземпляр Luna: все актуальные бинарники используют общий `/tmp/luna.lock`, и новый запуск завершает предыдущий — из какой бы папки он ни стартовал.

## Станции

Полный манифест моделей, бинарей и целей — [`luna-manifest.json`](luna-manifest.json).

| Станция | Codename | Цель сборки |
| :-- | :-- | :-- |
| `yandexmini3` | `bergamot` | `aarch64-unknown-linux-musl` |
| `yandexmini` | `yandexmini` | `aarch64-unknown-linux-musl` |
| `yandexmini_2` | `yandexmini_2` | `aarch64-unknown-linux-musl` |
| `yandexmicro` | `yandexmicro` | `aarch64-unknown-linux-musl` |
| `yandexmidi` | `cucumber` | `aarch64-unknown-linux-musl` |
| `yandexplum` | `plum` | `aarch64-unknown-linux-musl` |
| `yandexstation_2` | `station2` | `aarch64-unknown-linux-musl` |
| `pickle` | `pickle` | `aarch64-unknown-linux-musl` |
| `mango` | `mango` | `armv7-unknown-linux-musleabihf` |
| `yandexstation_3` | `orion` | `aarch64-unknown-linux-musl` |

Громкость Mini 2 на прошивке `0.369.1.46.4793643989.20261020.299` описана отдельно: [условия и проверка](docs/MINI2_VOLUME_046.md).

## API

HTTP API слушает `8099`:

```sh
BASE=http://192.168.1.50:8099/api

curl "$BASE/device"                     # модель, capabilities, топология LED
curl -X POST "$BASE/led/ring" \
  -H 'Content-Type: application/json' \
  -d '{"colors":["#FF0000","#0000FF"]}'
curl -X POST "$BASE/volume/set" \
  -H 'Content-Type: application/json' \
  -d '{"percent":20}'
```

Все методы, форматы и ошибки — в [API.md](docs/API.md).

## Разрешения

Мод объявляет permissions в manifest. Всё, что не запрошено, недоступно. Пользовательский Lua настраивается отдельно и по умолчанию выключен.

| Группа | Permissions |
| :-- | :-- |
| Индикация | `led`, `display`, `clock` |
| Звук | `volume`, `audio`, `microphone` |
| Ввод и датчики | `input`, `sensors` |
| Связь | `bluetooth`, `network`, `ipc.read`, `ipc.control` |
| Система | `commands`, `system`, `exec`, `web` |

У `network` есть дополнительный список хостов: включённое разрешение без непустого `network_allow` не даёт доступа в сеть. Подробности — в [API.md](docs/API.md).

## Веб-панель

При включённых `web.ui` и `web.api` интерфейс доступен по корню станции:

```text
http://192.168.1.50:8099/
```

Там LED-визуализатор, громкость, часы, Bluetooth, настройки, установка `.yap` и раздел «Тест API». Плагин с permission `web` добавляет свою вкладку через `register.customweb("web/index.html")`.

Сторонние панели не рисуют свой дизайн: Luna отдаёт общий стиль `GET /luna-panel.css`. Требования и пример страницы — в [WEB_PANELS.md](docs/WEB_PANELS.md).

Анонимная статистика выключена по умолчанию: панель спрашивает согласие при первом открытии, состав отчёта — в [TELEMETRY.md](docs/TELEMETRY.md).

## Документация

| Документ | О чём |
| :-- | :-- |
| [docs/README.md](docs/README.md) | оглавление всей документации по задачам |
| [GETTING_STARTED.md](docs/GETTING_STARTED.md) | от установки до первого пакета |
| [PACKAGE_GUIDE.md](docs/PACKAGE_GUIDE.md) | как написать пакет на Luna |
| [PLUGINS.md](docs/PLUGINS.md) | Lua-плагины и бандлы через YAM |
| [API.md](docs/API.md) | HTTP API, разрешения, внешние запросы |
| [WEB_PANELS.md](docs/WEB_PANELS.md) | требования к сторонним веб-панелям |
| [TELEMETRY.md](docs/TELEMETRY.md) | анонимная статистика: согласие и состав отчёта |
| [DEVICE_GUIDE.md](docs/DEVICE_GUIDE.md) | как добавить новую станцию |
| [EXTENDING_STATION.md](docs/EXTENDING_STATION.md) | как доработать поддержанную станцию |
| [MEDIA_PROVIDERS.md](docs/MEDIA_PROVIDERS.md) | медиапровайдеры и адаптеры плееров |
| [YAM-BUNDLES.md](docs/YAM-BUNDLES.md) | упаковка и запуск бандлов |
| [AIRPLAY_RUNTIME_CONTROLS.md](docs/AIRPLAY_RUNTIME_CONTROLS.md) | управление AirPlay во время работы |
| [LED_COLOUR.md](docs/LED_COLOUR.md) · [GENERIC-LED-CALIBRATION.md](docs/GENERIC-LED-CALIBRATION.md) | цвет и калибровка LED |

## Структура репозитория

| Путь | Назначение |
| :-- | :-- |
| `src/` | gateway, HTTP API, Lua runtime, обновления, медиа и веб-панель |
| `stations/<device>/` | station crate: backend и capability-функции модели |
| `web/` | UI панели, `/luna-panel.css` и общие ресурсы |
| `docs/` | документация |
| `tools/` | сборка, релиз, postinstall |
| `tests/` | Python-проверки установки и HTTP-контрактов |
| `luna-manifest.json` | станции, бинари и цели сборки |
| `yam.yaml` | описание пакета YAM |

## Разработка

Проверка workspace:

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
```

То же запускает GitHub Actions на каждый push и pull request: форматирование, тесты, Python-проверки из `tests/`, разбор JavaScript в `web/` и кросс-сборку каждой станции под её musl-target.

Сборка и установка станции на подключённое устройство:

```sh
tools/build-deploy.sh --device yandexmini3
```

Новая станция получает папку `stations/<device>/` со своим `station_main.rs`, hardware backend и capability-функциями. В общем gateway не должно быть модельных имён, числа LED, путей sysfs и адресов I²C.

## Безопасность

- API рассчитан на доверенную локальную сеть: аутентификации нет.
- Запросы из браузера с чужих сайтов отклоняются (`403`): проверяются `Host` (защита от DNS rebinding) и `Origin`.
- Для разработки слушайте только loopback: `luna --listen 127.0.0.1:8100`. `0.0.0.0` открывает UI и API всей сети.
- Внешние HTTP(S)-запросы из API и модов по умолчанию выключены и ограничены списком хостов; локальные адреса запрещены всегда.

## Участие

Issues и pull requests приветствуются. Перед отправкой:

1. `cargo fmt --all` и `cargo test --workspace --locked` проходят локально;
2. в изменениях общего gateway нет модельных имён, числа LED и путей sysfs;
3. новая возможность проверена на двух станциях с разным железом — или описано, что осталось непроверенным.

## Лицензия

[MIT](LICENSE) © Luna contributors
