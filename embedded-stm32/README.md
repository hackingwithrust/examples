# firmware — embedded Rust on STM32

Track: <https://hackingwithrust.com/projects/embedded-stm32/>

**Status: article 3 of 10.** The firmware drives PA5 — the board's user LED, LD2 —
high for 500 ms and low for 500 ms, in a loop. The next step looks at what `no_std`
costs.

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
