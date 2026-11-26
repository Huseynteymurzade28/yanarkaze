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
| Nordic nRF52840-DK       | nRF52840, Cortex-M4F 64 MHz    | J-Link VCP (UARTE0)     | builds, untested         |
| ESP32-C3                 | RISC-V RV32IMC 160 MHz         | built-in USB Serial/JTAG| builds, untested         |

## Architecture

```
core/                    yanarkaze-core: no_std, no hardware
  platform.rs            the traits a board implements: Led, Button, Sensor, Watchdog, System
  shell.rs               command table, built-in commands, event printing
  editor.rs              line editor: cursor keys, history, tab completion (pure, unit tested)
  fun.rs                 reaction game, dice, timers, mandelbrot, life, matrix, dump, ferris
  led.rs button.rs       task logic, generic over the traits
  sensor.rs watchdog.rs
  state.rs               shared state: Signal, Channel, Mutex, task statistics
boards/
  sim/                   std: raw-mode terminal, threads stand in for hardware
  nucleo-l452re/         embassy-stm32
  rp-pico/               embassy-rp + embassy-usb
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
  led <mode>           on | off | blink [ms] | heartbeat | morse <text>
  temp                 on-chip temperature (and supply voltage)
  watch                live temperature graph (press any key to stop)
  alert <celsius>      set the temperature alert threshold
  uptime               time since boot
  history              list the previous command lines
  echo <text>          print the text
  hang                 freeze the executor -> watchdog resets the system
  reboot               restart
  clear                clear the screen

Fun:
  reaction             reflex game: hit the button when the LED turns on
  roll [N]dM           roll dice with the RNG (roll 2d6, roll d20)
  timer <s> [label]    spawn a new countdown task at runtime
  mandel               Mandelbrot set, timed in CPU cycles
  life                 Conway's Game of Life
  matrix               digital rain
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

### Raspberry Pi Pico

Requires [picotool](https://github.com/raspberrypi/picotool). Hold BOOTSEL while plugging the
Pico in, then:

```sh
cd boards/rp-pico
cargo run --release              # flashes over USB and reboots
picocom /dev/ttyACM0             # the Pico shows up as a USB serial port
```

The Pico has no user button, so BOOTSEL doubles as the button once the firmware runs.

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
4. `mandel` on the Nucleo and on the Pico: hardware FPU vs. software floats.
5. `hang` blocks the cooperative executor. Nobody feeds the watchdog, the system resets and
   the banner reports why.

## Notes

- `stm32-metapac` 21.0 swaps the `CH17SEL`/`CH18SEL` bits of `ADC_CCR` for STM32L4. Per RM0394
  the temperature sensor is bit 23, so the Nucleo board sets it by hand.
- The Nucleo console uses DMA for TX and a circular DMA buffer for RX, so no input bytes are
  lost while a command is running.
