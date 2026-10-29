# Как добавить новую станцию в Luna

Этот документ предназначен для разработчиков, которые добавляют в Luna новую
модель станции или исправляют поддержку уже существующей.

Главное правило: gateway не знает конкретное железо. Он предоставляет единый
API, permissions, lifecycle пакетов и HTTP-слой. Аппаратная специфика живёт
только в crate конкретной станции.

~~~text
пакет Lua / HTTP API
        │
        ▼
Luna gateway и общий capability-контракт
        │
        ▼
stations/<device>/ — station crate
        │
        ▼
sysfs / framebuffer / I²C / evdev / Quasar IPC / ALSA
~~~

Новая станция должна добавляться так, чтобы общий код Luna не пришлось менять
из-за нового sysfs-пути, числа светодиодов, I²C-адреса или расположения кнопки.

## 1. Идентификаторы станции

У станции есть три разных идентификатора:

| Поле | Пример | Назначение |
|---|---|---|
| device | yandexmini3 | стабильный ID crate и сборочной матрицы |
| codename | bergamot | короткое имя аппаратной платформы для YAM/релизов |
| models | yandexmini3, yandex-mini3 | значения, которые могут встретиться при детекте |

device нельзя переименовывать после публикации бинарей. Новые варианты
обнаружения добавляйте в models, не создавая второй crate без аппаратной
причины.

Отдельный crate нужен, если различается хотя бы одно:

- ABI или target toolchain;
- драйвер LED, порядок цветов или физическая раскладка;
- I²C/SPI/framebuffer/sysfs-интерфейс;
- input device, коды кнопок или последовательности жестов;
- clock backend;
- способ записи микрофона или работы с локальным Quasar.

Если железо полностью совпадает, переиспользуйте существующий crate и добавьте
новое имя в models.

## 2. Соберите данные со станции

Не определяйте target и capabilities по названию модели или по похожей колонке.
Сохраните полный отчёт:

~~~sh
adb shell 'uname -a; uname -n; uname -m'
adb shell 'cat /proc/device-tree/compatible 2>/dev/null | tr "\000" "\n"'
adb shell 'cat /proc/cpuinfo'
adb shell 'cat /proc/bus/input/devices'
adb shell 'ls -la /dev/input /dev/i2c* /dev/fb* /dev/snd 2>/dev/null'
adb shell 'cat /proc/asound/cards 2>/dev/null; cat /proc/asound/pcm 2>/dev/null'
adb shell 'find /sys/class/leds -maxdepth 2 -type f 2>/dev/null | sort'
adb shell 'ps'
~~~

Для каждой функции зафиксируйте поведение, а не только путь:

- LED: зоны, количество, channel order, физический порядок и trigger;
- кнопки: device name, EV_KEY/EV_SW, code press/release и value;
- жесты: точная последовательность и максимальная задержка;
- clock: имена сегментов, порядок цифр, яркость и выключение;
- microphone: источник, формат, Quasar/IOHub и безопасная остановка;
- audio: штатный mediad/Quasar-маршрут, а не самодельный aplay;
- Bluetooth: штатный Quasar/maind стек, если он есть.

Команды и результаты тестов приложите к issue или pull request. Один успешный
эксперимент на похожей станции не является доказательством для новой модели.

## 3. Создайте station crate

Структура:

~~~text
stations/yandexnew/
├── Cargo.toml
├── station_main.rs
├── device.rs
├── hardware.rs
├── clock.rs                 # если есть собственные часы
└── microphone.rs            # если нужен свой microphone backend
~~~

station_main.rs должен быть только точкой входа:

~~~rust
mod device;

fn main() -> anyhow::Result<()> {
    luna::run_with_station(Some(device::spec()))
}
~~~

Cargo.toml:

~~~toml
[package]
name = "luna-yandexnew"
version = "0.2.7"
edition = "2021"

[[bin]]
name = "luna-yandexnew"
path = "station_main.rs"

[dependencies]
anyhow = "1"
libc = "0.2"
serde_json = "1"
luna = { path = "../.." }
~~~

Добавляйте зависимости только для конкретного backend. Общий gateway не должен
получать station-specific зависимости.

## 4. Опишите StationSpec

device.rs является аппаратным контрактом:

~~~rust
use luna::{LedTopology, StationSpec};

const CAPABILITIES: &[&str] = &[
    "led.ring.rgb",
    "led.ring.24",
];

pub fn spec() -> StationSpec {
    StationSpec {
        model: "yandexnew",
        led_profile: "new-ring",
        led_count: 24,
        led_topology: LedTopology {
            ring_count: 24,
            has_logo: false,
            logo_count: 0,
        },
        color_order: luna::ColorOrder::Rgb,
        create_led_backend: hardware::create_led_backend,
        create_microphone_backend: || None,
        create_clock_backend: || None,
        capabilities: CAPABILITIES,
        input_actions: &[],
        input_gestures: &[],
        gpio_inputs: &[],
    }
}
~~~

Поля:

- model — внутреннее имя, совпадающее с manifest;
- led_profile — имя аппаратного профиля, не место для hardware-логики;
- led_count — размер основной LED-зоны для совместимости;
- led_topology — реальная геометрия ring/logo;
- color_order — порядок каналов, который требует железо;
- create_*_backend — конструкторы station-specific реализаций;
- capabilities — только фактически проверенные возможности;
- input_actions, input_gestures и gpio_inputs — описания входов.

Не объявляйте capability авансом. Если backend отсутствует или не проверен,
пусть device API вернёт false, а не успешный вызов с последующим падением.

## 5. Реализуйте LED backend

Общий контракт:

~~~rust
pub trait LedBackend: Send + Sync {
    fn show(&self, frame: &[u8]) -> anyhow::Result<()>;

    fn restore_ring_trigger(&self) -> anyhow::Result<()> { Ok(()) }
    fn restore_logo_trigger(&self) -> anyhow::Result<()> { Ok(()) }

    fn show_logo(&self, _frame: &[u8]) -> anyhow::Result<()> {
        anyhow::bail!("logo LED is not supported by this station")
    }
}
~~~

Backend должен:

1. проверять наличие устройства и geometry при открытии;
2. проверять размер frame перед записью;
3. учитывать Rgb/Bgr и физический порядок LED;
4. не менять чужие trigger без явного требования hardware;
5. восстанавливать штатные trigger в restore_ring_trigger и restore_logo_trigger,
   если ручная яркость их отключает;
6. возвращать контекстную ошибку open/write/I²C.

Для ring и logo реализуйте отдельные topology и методы. Например:

~~~rust
led_topology: LedTopology {
    ring_count: 24,
    has_logo: true,
    logo_count: 6,
}
~~~

Равномерное распределение цветовой схемы выполняет gateway. Station backend
отвечает только за преобразование логического frame в физическую запись.

## 6. I²C и ioctl

Тип второго аргумента libc::ioctl отличается между target-платформами:

~~~rust
#[cfg(all(target_os = "linux", target_env = "musl"))]
const I2C_RDWR: libc::c_int = 0x0707;

#[cfg(any(
    not(target_os = "linux"),
    all(target_os = "linux", not(target_env = "musl"))
))]
const I2C_RDWR: libc::c_ulong = 0x0707;
~~~

Не исправляйте такую ошибку случайным as i32 или as u64. Проверьте сигнатуру
libc для каждого target. ABI userspace станции важнее архитектуры ядра.

## 7. Кнопки, GPIO и жесты

Для /dev/input/event* используйте InputAction:

~~~rust
const INPUT_ACTIONS: &[luna::InputAction] = &[
    luna::InputAction {
        name: "volume_up",
        device: "aw933xx",
        kind: 1, // EV_KEY
        code: 115,
    },
];
~~~

Для последовательностей используйте InputGesture:

~~~rust
const INPUT_GESTURES: &[luna::InputGesture] = &[
    luna::InputGesture {
        name: "minus-to-plus",
        actions: &["volume_down", "activate", "volume_up"],
        max_gap_ms: 300,
    },
];
~~~

Если последовательность уже подтверждена station crate, не заставляйте Lua
пакет собирать свайп из сырых событий.

Для GPIO-кнопок используйте GpioInput только после проверки active level:

~~~rust
const GPIO_INPUTS: &[luna::GpioInput] = &[
    luna::GpioInput {
        name: "mute",
        path: "/sys/class/gpio/gpio414/value",
        active_level: 0,
        code: 113,
        action: "mute",
    },
];
~~~

Если mute/unmute приходят отдельными GPIO-состояниями, описывайте их явно.
Одно событие не всегда содержит финальное состояние mute.

## 8. Clock и microphone

Clock backend должен обнаруживать только свою реальную разметку:

~~~rust
pub fn create_clock_backend() -> Option<Box<dyn luna::ClockBackend>> {
    let clock = Clock::detect()?;
    Some(Box::new(clock))
}
~~~

detect() должен вернуть None, если отсутствует обязательный сегмент. Нельзя
создавать часы только потому, что модель похожа на другую.

MicrophoneBackend добавляйте только при подтверждённом station-specific
способе записи. Если запись уже ведёт Quasar, используйте его IPC и не
открывайте микрофонный device параллельно.

## 9. Не дублируйте общие capabilities

В station crate не нужно копировать:

- распределение цветов и HTTP routes;
- device.has_* и counts;
- volume API;
- воспроизведение через штатный mediad;
- Quasar speech/input IPC;
- Bluetooth через штатный station stack;
- permissions, runtime tokens и plugin lifecycle.

Station crate предоставляет только hardware contract и backend. Новый общий
capability сначала добавляется в Luna как абстракция, затем подключается
станцией. Не добавляйте match model в gateway.

## 10. Добавьте запись в luna-manifest.json

~~~json
{
  "device": "yandexnew",
  "codename": "newname",
  "yam_target": "newname",
  "manifest_path": "stations/yandexnew/Cargo.toml",
  "models": ["yandexnew", "yandex-new"],
  "binary": "luna-yandexnew",
  "asset": "luna-yandexnew-aarch64-linux-musl",
  "target": "aarch64-unknown-linux-musl"
}
~~~

Проверьте:

- manifest_path находится внутри stations/<device>;
- binary совпадает с Cargo [[bin]] name;
- asset уникален и является одним именем файла;
- target соответствует реальному ABI;
- models не пересекается с другой записью;
- yam_target совпадает с platform ID актуального YAM repository.

Архитектура сама по себе не выбирает станцию: два устройства могут иметь
один aarch64, но разные hardware capabilities.

## 11. Сборка

Одна станция:

~~~sh
cargo build --release --locked \
  --target aarch64-unknown-linux-musl \
  --manifest-path stations/yandexnew/Cargo.toml \
  --bin luna-yandexnew
~~~

Все станции:

~~~sh
tools/build-stations.sh
~~~

Одна запись manifest:

~~~sh
tools/build-stations.sh luna-manifest.json dist yandexnew
~~~

Скрипт использует target из luna-manifest.json и не угадывает ABI по имени.
Если target или linker отсутствует, исправьте toolchain, а не заменяйте ABI
на ближайший.

## 12. Host-тесты

До прошивки запускайте:

~~~sh
cargo fmt --all -- --check
cargo test --workspace --locked
git diff --check
~~~

Тестами должны быть покрыты чистые части backend:

- преобразование цветов и порядок LED;
- проверка geometry и размера frame;
- декодирование input event для 32/64-bit;
- validation clock segments;
- manifest и compatibility.

Host-тест не доказывает наличие конкретного /dev, sysfs-пути или framebuffer
на станции. Hardware acceptance обязателен отдельно.

## 13. Тестирование на реальной станции

Перед запуском остановите старый Luna и проверьте, что нет двух процессов,
пишущих в один hardware backend:

~~~sh
adb shell 'pkill luna 2>/dev/null || true'
adb push target/<target>/release/luna-yandexnew /tmp/luna-yandexnew
adb shell 'chmod 755 /tmp/luna-yandexnew'
adb shell '/tmp/luna-yandexnew --listen 0.0.0.0:8100'
~~~

Проверка API:

~~~sh
curl -i http://<IP Станции>:8100/api/device
curl -i -X POST http://<IP Станции>:8100/api/led \
  -H 'Content-Type: application/json' \
  -d '{"colors":["#ff0000"]}'
~~~

Проверяйте по порядку:

1. /api/device: model, architecture, features и topology;
2. LED: один цвет, два цвета, максимум зоны и off;
3. input: сырые события и готовые gestures;
4. clock: time, segments и off;
5. volume и mute;
6. audio через mediad, без aplay;
7. microphone/Quasar IPC, если backend объявлен.

Записывайте команду, HTTP-код, ответ и фактическое поведение железа. Успешный
HTTP 200 без изменения станции — ошибка backend.

## 14. Частые ошибки

### Использование backend другой модели

Даже похожий LED-драйвер может иметь другой order, trigger или ABI. Сначала
подтвердите железо, затем переиспользуйте код внутри station crate.

### Capability объявлена без backend

Удалите capability из CAPABILITIES или реализуйте backend. /api/device должен
описывать реальность, а не план.

### Перепутаны несколько цветов

Gateway отвечает за логическое равномерное распределение, station backend —
за физическую перестановку. Не исправляйте distribution в hardware-коде.

### Сборка проходит на macOS, но не на target

Проверьте libc-типы, cfg(target_os/target_env), linker и target ABI.

### Звук ломает Алису или Bluetooth

Не запускайте aplay, собственный A2DP или второй Bluetooth daemon поверх
Quasar. Используйте общий audio/IPC backend и штатный maind/mediad стек.

### Устройство не поддерживается установщиком

Сравните uname -n с models в luna-manifest.json. Не добавляйте широкие
значения вроде yandex или aarch64.

## Рекомендуемые варианты реализации

Этот раздел необязательный. Он не задаёт обязательную архитектуру и не требует
повторять реализацию Luna или другой станции. Разработчик может выбрать любой
рабочий способ, если:

- capability корректно объявлена или скрыта;
- API отвечает предсказуемо;
- ошибки и недоступность не маскируются успешным ответом;
- реализация не ломает другие процессы станции;
- поведение описано в pull request.

### Bluetooth

Возможные варианты:

- использовать штатный Bluetooth/Quasar стек станции;
- использовать системный BlueZ, если он действительно доступен;
- реализовать station-specific backend через vendor IPC;
- предоставить только status/scan, если pair/connect не поддерживаются.

Важно проверить реальный результат операции, а не только код завершения команды.
Если подключение не удалось, endpoint должен вернуть ошибку. Нельзя объявлять
pair/connect доступными только потому, что на станции найден Bluetooth device.

### Команды Алисы и speech

В зависимости от прошивки можно использовать:

- штатные события aliced;
- локальный command spotter;
- vendor IPC;
- собственный recognizer, если он действительно нужен и не конфликтует со
  штатным микрофоном.

Нужно различать промежуточную и финальную расшифровку, источник события
(speech/command), фазу Alice и отмену. Если прошивка не умеет отменять ответ,
это следует явно вернуть пользователю, а не имитировать успешную отмену.

### Свои звуки

Возможные реализации:

- штатный FilePlayerCapability/mediad;
- vendor audio IPC;
- отдельный backend станции, если mediad отсутствует;
- прямой ALSA-путь, только если это реально единственный рабочий вариант.

Для любого варианта проверьте формат WAV, громкость, конкуренцию с Alice,
Bluetooth и системным audio output. Не копируйте библиотеки или пути другой
модели без проверки ABI.

### Громкость, microphone и прочие функции

Для каждой функции можно использовать штатный Quasar IPC, sysfs, evdev, GPIO,
ALSA, vendor daemon или собственный backend. Выбор остаётся за автором
станции. Важно сохранить общий контракт Luna:

~~~text
проверка доступности → вызов API → понятный результат или ошибка
~~~

Если реализация требует особого поведения, добавьте его в описание station
crate и приложите команды hardware-теста. Не добавляйте endpoint только ради
одной модели, если функцию можно выразить через существующий общий API.

## 16. Checklist pull request

- [ ] Есть stations/<device>/Cargo.toml и station_main.rs.
- [ ] Hardware logic находится только в station crate.
- [ ] Нет нового match model для hardware в src/gateway.rs.
- [ ] StationSpec содержит только проверенные capabilities.
- [ ] Указаны реальные topology, color order и counts.
- [ ] Проверены press/release, input actions и gestures.
- [ ] Clock/microphone возвращают None, если backend отсутствует.
- [ ] ABI и linker явно записаны в luna-manifest.json.
- [ ] Все models подтверждены выводом станции.
- [ ] Добавлены host-тесты чистой логики.
- [ ] Выполнены fmt, cargo test --workspace --locked и git diff --check.
- [ ] Выполнен hardware-тест на реальной станции.
- [ ] В PR приложены /api/device, target и результаты hardware-тестов.

