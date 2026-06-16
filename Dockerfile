FROM ubuntu:24.04 AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    fuse3 libfuse3-dev pkg-config build-essential ca-certificates curl libgit2-dev \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release && strip target/release/blossomfs

FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    fuse3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/blossomfs /usr/local/bin/blossomfs

ENTRYPOINT ["blossomfs"]
CMD ["mount", "--help"]
