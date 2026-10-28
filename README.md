# Luna

Luna — отдельный host-пакет для Lua, Quasar API, веб-интерфейса, старых `.yap`,
`plugin.toml` и их модели разрешений. Ядро `yam` не читает, не устанавливает и
не запускает такие плагины.

Сборка:

```sh
cargo build --release --locked
```

Затем укажите собранный бинарный файл для нужной платформы в `yam.yaml` и
соберите пакет командой `yam pack` из репозитория YAM.

`tools/convert_apkg.py` — офлайн-конвертер APKG в `.yap` только для этого
runtime. Он не создаёт и не устанавливает `.yampkg`.

## Комната устройства

Необязательная пользовательская метка принадлежит этому runtime, отдельно от
модели, аппаратных возможностей и метаданных пакетов YAM v2. Настройка доступна
в панели runtime, через `GET/PUT /api/room`, в `GET /api/device`, в авторизованном
runtime-ответе `/device` и в Lua как `device.room`.

Без запуска демона и обращения к аппаратуре:

```sh
luna --data-dir "$RUNTIME_DATA_DIR" room
luna --data-dir "$RUNTIME_DATA_DIR" room "Кухня"
luna --data-dir "$RUNTIME_DATA_DIR" room --clear
```

`RUNTIME_DATA_DIR` — тот же каталог, который сервис передаёт в `--data-dir`
(в `yam.yaml` это `${YAM_VAR}`). Это **не** корень пакетного менеджера `$YAM_ROOT`.
Команда ядра `yam room` не добавляется. Подробности, совместимость и проверки —
[docs/ROOMS.md](docs/ROOMS.md).
