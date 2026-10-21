# firmware — embedded Rust on STM32

Track: <https://hackingwithrust.com/projects/embedded-stm32/>

**Status: article 5 of 10.** The firmware reads PC13 — the board's user button, B1 —
and toggles PA5, the user LED LD2, on each accepted press. A press counts once the
line has held its new level for ten 1 ms ticks, so contact bounce does not register.
It carries its own panic handler: a panic lights LD2 and leaves it lit. The next step
dims the LED with a timer.

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
