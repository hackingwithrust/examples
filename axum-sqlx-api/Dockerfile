# ---- build stage: the toolchain, the whole source tree ----
FROM rust:1.99.0-slim AS build
WORKDIR /build

# Dependencies first, source last. Cargo.lock and the offline query cache
# (.sqlx/) go in before the code, so a source-only change reuses the layer
# that downloaded and compiled every dependency.
COPY Cargo.toml Cargo.lock ./
COPY .sqlx .sqlx

# A stub main compiles the dependency graph alone. The real main.rs, with
# the query! macros, replaces it after - the stub exists so the expensive
# layer can be cached without the source that changes on every edit.
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release

COPY src src
# sqlx::migrate! reads this directory at compile time - it is part of the
# binary, not a file the container ships.
COPY migrations migrations
RUN touch src/main.rs && cargo build --release

# ---- runtime stage: one binary, nothing else ----
FROM debian:trixie-slim
COPY --from=build /build/target/release/shortlink /usr/local/bin/shortlink
USER nobody
EXPOSE 8000
ENTRYPOINT ["/usr/local/bin/shortlink"]
