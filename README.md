<p align="center">
  <img src="assets/logo.png" alt="yanarkaze logo: a flame and a gust of wind in a circle" width="200">
</p>

# yanarkaze

```
                              _
 _  _  __ _  _ _   __ _  _ _ | |__ __ _  ___  ___
| || |/ _` || ' \ / _` || '_|| / // _` ||_ / / -_)
 \_, |\__,_||_||_|\__,_||_|  |_\_\\__,_|/__| \___|
 |__/
```

**yanar** (Azerbaijani: *burning*, as in [Yanardag](https://en.wikipedia.org/wiki/Yanar_Dag), the
mountain that has been on fire for millennia) + **kaze** (Japanese 風: *wind*).

A tiny async operating system written in Rust on top of the [Embassy](https://embassy.dev)
executor. It keeps burning on whatever board the wind carries it to: the same shell, tasks and
commands run on an STM32, an RP2040, an nRF52840, an ESP32-C3, and even in your desktop terminal.

## Platforms

| Board                    | Chip / core                    | Console                 | Status                   |
|--------------------------|--------------------------------|-------------------------|--------------------------|
| Desktop simulator        | your PC (Linux, macOS, Windows)| the terminal            | tested                   |
| ST NUCLEO-L452RE         | STM32L452, Cortex-M4F 80 MHz   | ST-LINK VCP (USART2)    | tested on hardware       |
| Raspberry Pi Pico        | RP2040, Cortex-M0+ 125 MHz     | native USB (CDC-ACM)    | builds, awaiting testing |
| Raspberry Pi Pico 2      | RP2350, Cortex-M33F 150 MHz    | native USB (CDC-ACM)    | tested on hardware       |
| Nordic nRF52840-DK       | nRF52840, Cortex-M4F 64 MHz    | J-Link VCP (UARTE0)     | builds, untested         |
| ESP32-C3                 | RISC-V RV32IMC 160 MHz         | built-in USB Serial/JTAG| builds, untested         |

## Architecture

```
core/                    yanarkaze-core: no_std, no hardware
  platform.rs            the traits a board implements: Led, Button, Sensor, Watchdog, System
  shell.rs               command table, built-in commands, event printing
  editor.rs              line editor: cursor keys, history, tab completion (pure, unit tested)
  keys.rs                decodes terminal bytes into keys for full-screen programs
  snake.rs               the rules of Snake, reporting only the cells that changed
  game2048.rs            the rules of 2048
  calc.rs                expression evaluator for `calc` (C operators, hex, binary)
  oled.rs                SSD1306 128x64 framebuffer, 5x7 font, text fitting, Game of Life
  hw.rs                  hardware commands: gpio, pwm, adc, i2c, oled
  fun.rs                 reaction game, snake, dice, timers, mandelbrot, life, matrix, dump, ferris
  led.rs button.rs       task logic, generic over the traits
  sensor.rs watchdog.rs
  state.rs               shared state: Signal, Channel, Mutex, task statistics
boards/
  sim/                   std: raw-mode terminal, threads stand in for hardware
  nucleo-l452re/         embassy-stm32
  rp-pico/               embassy-rp + embassy-usb (RP2040)
  rp-pico2/              embassy-rp + embassy-usb (RP2350, Arm cores)
  nrf52840-dk/           embassy-nrf
  esp32c3/               esp-hal + esp-rtos
```

The core never touches a register. A board crate implements five small traits and spawns
thin `#[embassy_executor::task]` wrappers around the core task functions:

```rust
#[embassy_executor::task]
async fn led_task(led: PicoLed) {
    yanarkaze_core::led::run(led).await
}
```

Porting to a new board is usually 200 to 300 lines: pick a console (anything implementing
`embedded_io_async::Read + Write`), an LED, a button, a temperature source and a watchdog.

### Tasks

| Task        | What it does                                           |
|-------------|--------------------------------------------------------|
| `shell`     | interactive command line                               |
| `led`       | LED patterns: blink, heartbeat, morse code             |
| `button`    | debounced button, cycles LED modes or feeds the game   |
| `sensor`    | samples the die temperature, raises threshold alerts   |
| `watchdog`  | feeds the hardware watchdog (2 s timeout)              |
| `countdown` | up to 4 spawned at runtime by the `timer` command      |

They talk through `embassy-sync` primitives: a `Signal` for the LED mode, a `Channel` of events
that the shell prints asynchronously, and mutex-protected shared readings.

## Shell commands

```
System:
  help [command]       show this help, or the help for one command
  info                 board, chip, clock, uptime and reset reason
  ps                   list running tasks and their wakeups
  top                  live task monitor with wakeups per second (any key stops)
  led <mode>           on | off | blink [ms] | heartbeat | morse <text>
  temp                 on-chip temperature (and supply voltage)
  watch                live temperature bar (press any key to stop)
  graph                live chart of the last minute's temperature (any key stops)
  alert <celsius>      set the temperature alert threshold
  uptime               time since boot
  history              list the previous command lines
  echo <text>          print the text
  calc <expr>          calculator: 0xff & ~0x0f, 1 << 12, 7 / 2, (3 + 4) * 2
  repeat <n> <cmd>     run a command n times (any key stops)
  every <s> <cmd>      run a command every s seconds (any key stops)
  hang                 freeze the executor -> watchdog resets the system
  reboot               restart
  bootloader           restart into the USB bootloader, ready for new firmware (RP boards)
  clear                clear the screen

Hardware (Pico 2 and the simulator; i2c and oled also on the NUCLEO):
  pins                 which pins can do what on this board
  gpio <pin> <action>  on | off | toggle | read [up|down]   e.g. gpio 16 on
  pwm <pin> <%> [hz]   PWM output, e.g. LED brightness: pwm 16 25   (pwm 16 off)
  fade <pin> [ms]      make an LED breathe (any key stops)
  adc <pin> [watch]    read an analog pin in millivolts, e.g. a potentiometer
  knob <adc> <pwm>     a potentiometer dims an LED, live: knob 26 16
  i2c scan|read|write  find I2C devices (and name the common ones), read or write registers
  oled <action>        SSD1306 OLED at 0x3C: text <msg> | temp | dash | life | logo | preview
                       | clear | invert | contrast <0-255> | on | off

Fun:
  reaction             reflex game: hit the button when the LED turns on
  roll [N]dM           roll dice with the RNG (roll 2d6, roll d20)
  timer <s> [label]    spawn a new countdown task at runtime
  mandel               Mandelbrot set, timed in CPU cycles
  life                 Conway's Game of Life
  matrix               digital rain
  snake                the classic, with arrow keys (the button pauses)
  2048                 slide and merge tiles with the arrow keys
  dump <addr> [len]    hex dump of real memory (dump with no args for examples)
  say <text>           Ferris the crab says it
  fortune              words of wisdom
```

Line editing keys:

| Key                          | Action                                   |
|------------------------------|------------------------------------------|
| Tab                          | complete commands and arguments (`led he<Tab>`) |
| left / right                 | move the cursor                          |
| Home, Ctrl+A / End, Ctrl+E   | jump to the start / end of the line      |
| Backspace / Delete           | delete before / under the cursor         |
| Ctrl+W / Ctrl+U / Ctrl+K     | delete the word before / to start / to end |
| up, Ctrl+P / down, Ctrl+N    | browse the history (the half-typed line is kept) |
| Ctrl+L                       | clear the screen                         |
| Ctrl+C                       | cancel the line                          |

Commands can be chained with `;` (`led on; temp`), and `repeat` / `every` wrap one
(`every 1 temp`, `repeat 5 roll d20`).

`help` and tab completion are generated from the command tables in `core/src/shell.rs`; a
test fails if a table entry has no handler.

## Running it

### Desktop simulator (no hardware)

```sh
cargo run -p yanarkaze-sim
```

Ctrl+B presses the button, Ctrl+T puts your finger on the chip (the simulated temperature
rises, try `alert 30` first), Ctrl+D quits. The LED blinks in the terminal's title bar, and
`hang` really freezes the executor: a watchdog thread restarts the process.

### NUCLEO-L452RE

Requires [probe-rs](https://probe.rs) (`cargo install probe-rs-tools --locked`).

```sh
cd boards/nucleo-l452re
cargo run --release              # flash and stream defmt logs
picocom -b 115200 /dev/ttyACM0   # in a second terminal: the shell
```

I2C1 is on the Arduino header, so an SSD1306 OLED (or any I2C sensor) plugs straight in:

| OLED pin | NUCLEO pin                                              |
|----------|---------------------------------------------------------|
| GND      | GND (CN6 power header, or CN5 just below AREF)          |
| VCC      | 3V3 (CN6 power header)                                  |
| SCL      | D15 = PB8 (CN5, the top pin, above SDA)                 |
| SDA      | D14 (CN5, the second pin from the top)                  |

D14 is PB9 on the NUCLEO-L452RE but PB7 on the NUCLEO-L452RE-P; the firmware finds out at
boot from the module's pull-ups, so attach the display before powering up.

Then `i2c scan` should find the display at 0x3C, the logo appears at the next boot, and
`oled dash`, `snake` and `2048` draw on it. I2C is blocking on this board, so a full OLED
frame pauses the other tasks for about 25 ms.

### Raspberry Pi Pico

Requires [picotool](https://github.com/raspberrypi/picotool). Hold BOOTSEL while plugging the
Pico in, then:

```sh
cd boards/rp-pico
cargo run --release              # flashes over USB and reboots
picocom /dev/ttyACM0             # the Pico shows up as a USB serial port
```

The Pico has no user button, so BOOTSEL doubles as the button once the firmware runs.

### Raspberry Pi Pico 2

Requires `rustup target add thumbv8m.main-none-eabihf`. Hold BOOTSEL while plugging the Pico 2
in, then either:

```sh
cd boards/rp-pico2
cargo run --release              # flashes with picotool
```

or, if picotool cannot reach the board (no udev rule), drag and drop a UF2:

```sh
cargo build --release
picotool uf2 convert target/thumbv8m.main-none-eabihf/release/yanarkaze-rp-pico2 -t elf \
    pico2.uf2 --family rp2350-arm-s
cp pico2.uf2 /run/media/$USER/RP2350/  # the drive the Pico 2 shows in BOOTSEL mode
picocom /dev/ttyACM0
```

Wiring for the hardware commands (`pins` prints the full list):

| Part                | Connect                                                             |
|---------------------|---------------------------------------------------------------------|
| LED                 | GP16 (pin 21) -> 220-330 ohm resistor -> LED long leg; short leg to GND (pin 23) |
| Potentiometer       | outer legs to 3V3 (pin 36) and AGND (pin 33), middle leg to GP26 (pin 31) |
| I2C (OLED, sensors) | SDA to GP4 (pin 6), SCL to GP5 (pin 7), VCC to 3V3 (pin 36), GND to GND (pin 38) |

Then try `fade 16`, `adc 26 watch`, `knob 26 16` and `i2c scan`. With a 128x64 SSD1306
OLED attached the logo appears at boot; try `oled text hello`, `oled dash` and `oled life`
(`oled preview` draws the framebuffer in the terminal, so it works without a display too).
`snake` and `2048` play on the OLED as well, alongside the terminal.

The RP2350's internal pull-downs suffer from erratum E9 (a pin that has been high holds
itself at about 2 V); `gpio <pin> read down` works around it by briefly disabling the input
buffer before reading.

After the first flash, the `bootloader` shell command brings the board back to BOOTSEL mode,
so updates need no replugging.

embassy-rp can only read the BOOTSEL button on the RP2040, so on the Pico 2 the button is
GPIO15 (pin 20) with the internal pull-up: touch it to GND (pin 18), or wire a push button
between them. `mandel` measures in CPU cycles here too, and the M33's FPU makes it fast
(about 9 ms for the whole set).

### nRF52840-DK

```sh
cd boards/nrf52840-dk
cargo run --release              # probe-rs through the on-board J-Link
picocom -b 115200 /dev/ttyACM0
```

### ESP32-C3

Requires `rustup target add riscv32imc-unknown-none-elf` and
[espflash](https://github.com/esp-rs/espflash) (`cargo install espflash --locked`).

```sh
cd boards/esp32c3
cargo run --release              # flashes and opens the serial monitor
```

The LED pin (GPIO8, active low) matches the common "SuperMini" boards; change `EspLed` in
`boards/esp32c3/src/main.rs` for other boards.

## Tests

```sh
cargo test -p yanarkaze-core
```

## Demo ideas

1. `led morse HELLO` and keep typing commands while the LED blinks: tasks run concurrently.
2. `alert 30`, then put your finger on the MCU: an `[alert]` event pops up in the shell.
3. `timer 5 tea`, `timer 8 coffee`, then `ps`: new tasks spawned while the system runs.
4. `mandel` on the Pico 2 and on the Pico: hardware FPU vs. software floats.
5. `snake`, then press the board's button: it pauses the game and the LED starts blinking.
   `top` in between shows which tasks woke up and how often.
6. `hang` blocks the cooperative executor. Nobody feeds the watchdog, the system resets and
   the banner reports why.

## Notes

- `stm32-metapac` 21.0 swaps the `CH17SEL`/`CH18SEL` bits of `ADC_CCR` for STM32L4. Per RM0394
  the temperature sensor is bit 23, so the Nucleo board sets it by hand.
- The Nucleo console uses DMA for TX and a circular DMA buffer for RX, so no input bytes are
  lost while a command is running.
