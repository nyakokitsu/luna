# Как добавить capability уже поддержанной станции

Этот гайд нужен, когда станция уже есть в Luna, но у неё не работает или ещё
не описана отдельная возможность: logo, часы, кнопки, жесты, звук, микрофон и
тому подобное.

Для новой модели с нуля смотрите [DEVICE_GUIDE.md](DEVICE_GUIDE.md). Здесь не
нужно создавать новый crate или копировать рабочую реализацию целиком.

## Главное правило

Если station crate уже существует, дорабатывайте его:

```text
stations/<device>/
├── station_main.rs
├── device.rs       # StationSpec и capabilities
├── hardware.rs     # LED, I²C, sysfs
├── clock.rs        # если нужны часы
└── microphone.rs   # если нужен отдельный backend
```

Общий `src/gateway.rs` не должен узнавать новую модель, число LED, путь sysfs
или код кнопки. Gateway уже знает общий контракт, а crate станции сообщает,
как этот контракт выполнить.

## 1. Зафиксировать, что именно не работает

Перед изменениями соберите данные с реальной станции:

```sh
adb shell 'uname -a; uname -n; uname -m'
adb shell 'cat /proc/device-tree/compatible 2>/dev/null | tr "\000" "\n"'
adb shell 'cat /proc/bus/input/devices'
adb shell 'ls -la /dev/input /dev/i2c* /dev/fb* /dev/snd 2>/dev/null'
adb shell 'find /sys/class/leds -maxdepth 2 -type f 2>/dev/null | sort'
adb shell 'cat /proc/asound/cards 2>/dev/null; cat /proc/asound/pcm 2>/dev/null'
```

Для capability нужен воспроизводимый факт:

- LED — точные зоны, количество, порядок цветов и штатный `trigger`;
- кнопки — имя input-устройства, `EV_KEY`/`EV_SW`, код, press/release;
- жесты — последовательность действий и допустимая задержка;
- часы — реальные сегменты, порядок цифр и способ выключения;
- звук — штатный Quasar/mediad-маршрут и формат файла;
- микрофон — подтверждённый способ записи и безопасная остановка.

Сохраните команды и вывод в issue или pull request. Не переносите реализацию
с похожей станции только по названию модели.

## 2. Найти существующую точку подключения

Сначала откройте `stations/<device>/device.rs`. Там обычно находятся:

- `led_topology` и `color_order`;
- `create_led_backend`;
- `create_clock_backend`;
- `create_microphone_backend`;
- `capabilities`;
- `input_actions`, `input_gestures` и `gpio_inputs`.

Если нужный backend уже есть, исправьте его и добавьте тест. Если backend уже
создаётся, но capability не видна, проверьте `StationSpec` и список
`capabilities`.

Пример: для добавления logo LED меняются `LedTopology`, capability и методы
существующего LED backend. Новый общий код в gateway для этого не нужен:

```rust
led_topology: LedTopology {
    ring_count: 6,
    has_logo: true,
    logo_count: 1,
},
```

Числа должны быть измерены на станции, а не взяты из другой модели.

## 3. Добавить реализацию

### Аппаратная функция

Аппаратная логика должна оставаться в crate станции. Для sysfs/I²C/backend:

1. проверьте устройство при открытии;
2. проверьте размер и формат данных перед записью;
3. верните понятную ошибку, если capability недоступна;
4. не меняйте соседние устройства и чужие `trigger`;
5. восстановите штатное состояние после `off`, если запись отключила trigger.

### Кнопка или жест

Для одной кнопки добавьте `InputAction` с реальным device name и кодом. Для
полного жеста добавьте `InputGesture`, чтобы пакет получал одно понятное
действие, а не собирал последовательность Linux-событий самостоятельно:

```rust
InputGesture {
    name: "minus-to-plus",
    actions: &["volume_down", "activate", "volume_up"],
    max_gap_ms: 300,
}
```

Не называйте `KEY_VOICECOMMAND` кнопкой `pause`, если на станции это действие
означает `activate`. Имена должны описывать смысл события.

### Звук и Quasar

Если станция использует штатный Quasar, подключайте capability через уже
существующий IPC/mediad backend. Не добавляйте `aplay`, второй ALSA-микшер или
собственный аудиодемон: это может рассинхронизировать Алису и громкость.

## 4. Обновить описание capability

Capability добавляется только после проверки backend:

```rust
const CAPABILITIES: &[&str] = &[
    "led.logo.rgb",
    "led.logo.6",
];
```

Если функция физически отсутствует, не добавляйте её для удобства. Тогда
`/api/device` и Lua корректно покажут её отсутствие через `device.has_*` или
`is_available()`.

## 5. Написать тесты

Тест должен проверять не только успешную запись, но и безопасный отказ:

- правильный порядок и количество LED;
- неправильную geometry;
- неизвестный trigger;
- отсутствие обязательного sysfs-файла;
- press/release и распознавание жеста;
- восстановление штатного LED trigger после `off`.

Для station crate используйте обычные unit-тесты. Hardware-тесты, требующие
реальную станцию, помечайте отдельно и описывайте команду запуска.

## 6. Проверить общий контракт

После изменений запустите:

```sh
cargo test --workspace --locked
cargo build --release --locked \
  --target <target> \
  --manifest-path stations/<device>/Cargo.toml \
  --bin luna-<device>
```

На станции проверьте:

```sh
curl http://<IP-станции>:<порт>/api/device
```

Проверьте, что:

- capability появилась в `features` только когда она реально доступна;
- `has_*` и counts соответствуют физическому устройству;
- старые endpoint’ы не изменили поведение;
- отсутствие новой функции возвращает понятный `400`, а не падение процесса;
- существующие LED, часы, input и звук не сломались.

Для Lua-пакета минимальная проверка выглядит так:

```lua
if device.has_logo then
  alice.led.set.logo({"#00aaff"})
  alice.sleep(1000)
  alice.led.off.logo()
end
```

## 7. Что должно попасть в pull request

В PR приложите:

1. модель станции и версию прошивки;
2. вывод команд обнаружения устройства;
3. описание проверенного поведения;
4. изменённые файлы `stations/<device>/`;
5. тесты и результат `cargo test --workspace --locked`;
6. пример ответа `/api/device` до и после изменения, если меняется список
   capabilities.

Если изменение относится только к одной станции, не добавляйте общий `match`
по модели в gateway. Если выяснилось, что capability нужна нескольким моделям,
сначала оформите общий trait/контракт, а затем подключите его в их station
crate.

## Связанные документы

- [Как добавить станцию с нуля](DEVICE_GUIDE.md)
- [Начало работы с Luna](GETTING_STARTED.md)
- [Полный HTTP API](API.md)
- [Как написать пакет](PACKAGE_GUIDE.md)
