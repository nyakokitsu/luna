# Lua-плагины через YAM

YAM хранит файлы пакета, а Luna загружает и запускает Lua-плагин. Это
разделение важно: Luna не меняет файлы, которыми владеет YAM, и не пытается
самостоятельно обновлять пакет.

Плагин размещается в общем дереве YAM:

```text
share/luna/plugins/<namespace>.<name>/
├── plugin.toml
└── main.lua
```

Для пакета `vasya/clock` каталог будет называться `vasya.clock`. Это имя
должно совпадать с `id` в `plugin.toml`.

## Минимальный пакет

`yam.yaml`:

```yaml
name: vasya/clock
version: 1.0.0
license: MIT
description: Часы на кольце
targets: [any]
depends: [yam/luna ^0.2]
files:
  share/luna/plugins/vasya.clock/: plugin/
```

`plugin/plugin.toml`:

```toml
id = "vasya.clock"
name = "Часы"
version = "1.0.0"
entry = "main.lua"
permissions = ["led"]

[compatibility]
features = ["led.ring.rgb"]
```

`plugin/main.lua`:

```lua
if device.has_ring then
  alice.led.set.ring({"#00aaff"})
end
```

Для чистого Lua-пакета используйте `targets: [any]`: в нём нет native-бинаря,
поэтому CPU и libc станции не имеют значения. Пакету с native-файлами нужны
отдельные targets и артефакты под соответствующие платформы.

Актуальный формат упаковки описан в [документации YAM](https://github.com/nyakokitsu/yam/blob/main/docs/PACKAGING.md).

## Своя вкладка в веб-панели

Плагин может добавить HTML-страницу в Luna Web UI. Для этого нужны permission
`web`, режим сервиса и файл внутри каталога плагина:

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

Страница появится отдельной вкладкой и сможет обращаться к API Luna через
`fetch('/api/...')`. Разрешены только `.html` и `.htm` из каталога пакета,
размер страницы — до 4 МБ. Регистрация существует, пока работает плагин.

## Как Luna видит каталог YAM

Luna ищет его в следующем порядке:

1. путь из `--yam-root`;
2. переменная `YAM_ROOT`;
3. путь рядом с установленным бинарём вида
   `$YAM_ROOT/pkgs/yam/luna/<version>/bin/luna`.

Для синхронизации с транзакциями YAM передайте `--yam-run` или задайте
`YAM_RUN`. Тогда Luna использует `$YAM_RUN/yam.lock` как shared lock. Если
`YAM_ROOT` найден, а `YAM_RUN` нет, Luna продолжит работу, но предупредит, что
чтение каталога не синхронизировано с установкой.

Плагин из YAM отмечается в `GET /api/plugins` как `source: "yam"`. Такой пакет
нельзя удалить через Luna или заменить локальным `.yap`; используйте YAM:

```sh
yam remove vasya/clock
```

Состояние запуска и runtime-логи Luna хранятся отдельно, в
`<data-dir>/plugins/plugin-state/<id>/`.

Изменения в каталоге обнаруживаются при запуске, после
`POST /api/plugins/rescan` и во время фонового сканирования. Источники можно
посмотреть через `GET /api/plugins/sources`.

Binary-плагины из YAM намеренно не запускаются автоматически через Lua API.
Для них используется отдельный runtime-контракт и отдельные permissions.
