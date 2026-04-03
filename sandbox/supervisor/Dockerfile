FROM rust:slim-trixie

RUN apt-get update -qq \
    && apt-get install -y -qq capnproto libcapnp-dev musl-tools >/dev/null 2>&1 \
    && rustup target add aarch64-unknown-linux-musl \
    && rm -rf /var/lib/apt/lists/*
