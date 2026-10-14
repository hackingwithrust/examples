# firmware — embedded Rust on STM32

Track: <https://hackingwithrust.com/projects/embedded-stm32/>

**Status: not started.** The first commit lands with article 2, *Setting up the toolchain*.

Tags: `stm32-ch01` … `stm32-ch10`.

Needs the cross target and a flashing tool:

```sh
rustup target add thumbv7em-none-eabihf
cargo install probe-rs-tools
```

This crate does not build for the host. Use `cargo build --target thumbv7em-none-eabihf`.

See [`../README.md`](../README.md) for what the tags mean.
