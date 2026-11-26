# NucleOS

A tiny async "operating system" for the **NUCLEO-L452RE** (STM32L452, Cortex-M4F @ 80 MHz),
written in Rust on top of the [Embassy](https://embassy.dev) async executor.

Instead of threads with their own stacks, every subsystem is an `async` task. The CPU sleeps
whenever all tasks are waiting, and interrupts (timer, EXTI, DMA) wake exactly the task
that needs to run.

```
  _   _            _       ___  ____
 | \ | |_   _  ___| | ___ / _ \/ ___|
 |  \| | | | |/ __| |/ _ \ | | \___ \
 | |\  | |_| | (__| |  __/ |_| |___) |
 |_| \_|\__,_|\___|_|\___|\___/|____/
```

## Tasks

| Task       | What it does                                                              | Woken by            |
|------------|---------------------------------------------------------------------------|---------------------|
| `shell`    | Interactive command line on the ST-LINK virtual COM port                  | UART RX DMA, events |
| `led`      | LED patterns: blink, heartbeat, morse code                                | timer, mode signal  |
| `button`   | Blue USER button, debounced, cycles LED modes                             | EXTI interrupt      |
| `sensor`   | On-chip temperature sensor + VDDA via VREFINT, raises threshold alerts    | timer               |
| `watchdog` | Feeds the independent watchdog (IWDG, 2 s timeout)                        | timer               |

Tasks communicate through `embassy-sync` primitives:

- `Signal<LedMode>`: shell/button -> led (latest value wins, the pattern switches instantly)
- `Channel<Event, 8>`: button/sensor -> shell (button presses, temperature alerts)
- `blocking_mutex::Mutex`: shared sensor readings and state for `ps`

## Shell commands

```
help                 show this help
info                 chip, clock, uptime and reset reason
ps                   list running tasks and their wakeups
led <mode>           on | off | blink [ms] | heartbeat | morse <text>
temp                 on-chip temperature and supply voltage
watch                live temperature graph (press any key to stop)
alert <celsius>      set the temperature alert threshold
uptime               time since boot
hang                 freeze the executor -> watchdog resets the chip
reboot               software reset
clear                clear the screen
```

### Fun commands

Each one shows off a different part of the chip or the runtime (`src/fun.rs`):

```
reaction             reflex game: hit the button when the LED turns on   (EXTI timestamps)
roll [N]dM           roll dice, e.g. roll 2d6, roll d20                   (hardware TRNG)
timer <s> [label]    spawn a new countdown task at runtime                (task pool, pool_size = 4)
mandel               Mandelbrot set, timed in CPU cycles                  (FPU + DWT cycle counter)
life                 Conway's Game of Life seeded from the TRNG
matrix               digital rain
dump <addr> [len]    hex dump of flash, SRAM or factory data              (try: dump 0x1FFF7590 12)
say <text>           Ferris the crab says it
fortune              words of wisdom
```

## Demo ideas

1. `led morse HELLO` and keep typing commands while the LED blinks: tasks run concurrently.
2. `alert 30`, then put your finger on the MCU: an `[alert]` event pops up in the shell.
3. `watch` for a live temperature bar graph.
4. `hang` blocks the cooperative executor with a busy loop. Nobody feeds the watchdog,
   the chip resets after 2 s and the banner reports `last reset: watchdog (IWDG)`.
5. `ps` before and after pressing the button to see the wakeup counters grow.
6. `timer 5 tea`, `timer 8 coffee`, then `ps`: new tasks were spawned while the system runs.
7. `dump 0x1FFF7590 12` shows the chip's unique ID straight from factory memory; compare with `info`.

## Build and flash

Requirements: Rust with the `thumbv7em-none-eabihf` target and
[probe-rs](https://probe.rs) (`cargo install probe-rs-tools --locked`).

```sh
cargo run --release              # flash NucleOS and stream defmt logs over RTT
cargo run --release --bin blinky # the minimal blinky example
```

Connect to the shell (115200 8N1) with any serial terminal, for example:

```sh
picocom -b 115200 /dev/ttyACM0   # or: tio /dev/ttyACM0
```

## Notes

- The user LED is LD2 on PA5 (NUCLEO-L452RE) or LD4 on PB13 (NUCLEO-L452RE-P); both are driven.
- `stm32-metapac` 21.0 swaps the `CH17SEL`/`CH18SEL` bits of `ADC_CCR` for STM32L4.
  Per RM0394 the temperature sensor is bit 23, so `sensor_task` sets it by hand.
- The shell UART uses DMA for TX and a circular DMA buffer for RX, so no input bytes are lost
  while a command is running.
