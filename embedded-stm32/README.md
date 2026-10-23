# firmware — embedded Rust on STM32

Track: <https://hackingwithrust.com/projects/embedded-stm32/>

**Status: article 10 of 10.** Four embassy tasks run the board: a breath on PA5 (LD2), the
button on PC13 (B1), a BMP280 over I2C1 (PB8/PB9) and a logger on USART2 plus defmt-RTT. An
accepted press pauses or resumes the breath — a press counts once the line has held its new
level for ten 1 ms ticks, so contact bounce does not register — and the temperature sets how
fast the breath runs. Holding B1 for two seconds puts the chip in Stop mode: every clock
stops, the SRAM survives, and the EXTI line on PC13 brings it back. The firmware carries its
own panic handler: a panic lights LD2 and leaves it lit.

Tags: `stm32-ch02` … `stm32-ch10`.

Needs the cross target and a flashing tool:

```sh
rustup target add thumbv7em-none-eabihf
cargo install probe-rs-tools
```

Build and flash (the board is a NUCLEO-F401RE, chip `STM32F401RE`):

```sh
cargo build                          # .cargo/config.toml pins the target
cargo flash --chip STM32F401RE       # or plain `cargo run`, which flashes via the runner
```

This crate does not build for the host: `.cargo/config.toml` sets
`target = "thumbv7em-none-eabihf"` for every cargo command, so `cargo build` cross-compiles
without an explicit `--target`.

See [`../README.md`](../README.md) for what the tags mean.
