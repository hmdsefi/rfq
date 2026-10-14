# syntax=docker/dockerfile:1
# Builds one binary of the workspace with the Aeron feature, e.g.
#   docker build -f docker/rust.Dockerfile --build-arg BIN=rfq-node -t rfq-node .
# The build context is the repository root; docker/rust.Dockerfile.dockerignore keeps it to the
# manifests and crates/.

# --- toolchain: what building rusteron-archive needs (see docs/specs/transport-aeron.md) ---
# Pinned to the rust-toolchain.toml channel.
FROM rust:1.99.0-bookworm AS toolchain
# Aeron 1.53 needs CMake >= 3.30; bookworm ships 3.25, so cmake comes from bookworm-backports.
# clang/libclang for bindgen, a JDK for the Gradle step of the Aeron build, uuid/bsd to link.
RUN echo "deb http://deb.debian.org/debian bookworm-backports main" > /etc/apt/sources.list.d/backports.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends clang libclang-dev default-jdk-headless uuid-dev libbsd-dev pkg-config \
    && apt-get install -y --no-install-recommends -t bookworm-backports cmake \
    && rm -rf /var/lib/apt/lists/*
# rusteron's build script runs rustfmt on the generated bindings; the image's toolchain is minimal.
RUN rustup component add rustfmt clippy

# --- skeleton: the manifests plus empty target roots, so dependencies build in a cached layer ---
FROM debian:bookworm-slim AS skeleton
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN mkdir /skeleton && cp Cargo.toml Cargo.lock /skeleton/ \
    && find crates -name Cargo.toml -exec cp --parents {} /skeleton/ \; \
    && find crates \( -path '*/src/lib.rs' -o -path '*/src/main.rs' -o -path '*/src/bin/*.rs' \
         -o -path '*/benches/*.rs' -o -name build.rs \) -exec cp --parents {} /skeleton/ \; \
    && find /skeleton/crates -name '*.rs' ! -name lib.rs -exec sh -c 'echo "fn main() { }" > "$1"' _ {} \; \
    && find /skeleton/crates -name lib.rs -exec truncate -s 0 {} \;

# --- builder ---
FROM toolchain AS builder
ARG BIN
ARG FEATURES=aeron
WORKDIR /src
# Dependencies only (rusteron and the Aeron C build take minutes); cached until a manifest changes.
COPY --from=skeleton /skeleton/ ./
RUN test -n "$BIN" && cargo build --release --locked -p "$BIN" --features "$FEATURES"
COPY Cargo.toml Cargo.lock ./
COPY crates crates
# The stubs above are newer than the real sources' mtimes; touch so cargo rebuilds the workspace.
RUN find crates -name '*.rs' -exec touch {} + \
    && cargo build --release --locked -p "$BIN" --features "$FEATURES" \
    && mkdir /out && cp "target/release/$BIN" /out/app

# --- runtime ---
FROM debian:bookworm-slim
ARG BIN
# The binary links libbsd and libmd dynamically (Aeron C).
RUN apt-get update \
    && apt-get install -y --no-install-recommends libbsd0 libmd0 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /out/app /usr/local/bin/app
RUN ln -s app "/usr/local/bin/$BIN"
ENTRYPOINT ["/usr/local/bin/app"]
