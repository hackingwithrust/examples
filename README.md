# Track code

Working code for the project tracks at [hackingwithrust.com/projects](https://hackingwithrust.com/projects/).
This repository holds only the example code — the articles themselves live at
[hackingwithrust.com](https://hackingwithrust.com/).

```
cli-tool/           hit      — a command-line HTTP client
axum-sqlx-api/      — a short link API on Axum + SQLx
tauri-desktop-app/  — a Markdown notes app
react-wasm/         — a Rust function compiled to WebAssembly
embedded-stm32/     — firmware for an STM32 board
toy-database/       — an LSM key-value store
```

Clone just this repository and you have everything you need to follow along:

```sh
git clone git@github.com:hackingwithrust/examples.git
cd examples/cli-tool
cargo run -- --help
```

## Tags

Every article ends with a command like this:

```sh
cd cli-tool
git checkout cli-ch04
```

The tag is the state of the project **after** the article is finished. If your build fails and you
cannot see why, check out that tag and diff it against your own tree — that is what it is for.

Tag names are `<track>-ch<NN>`, zero-padded to two digits.

## Running a track

Each track is its own Cargo project with its own `README.md` and its own instructions.
