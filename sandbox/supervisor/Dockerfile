FROM rust:1.94-slim-trixie

RUN apt-get update -qq \
    && apt-get install -y -qq capnproto libcapnp-dev musl-tools >/dev/null 2>&1 \
    && rm -rf /var/lib/apt/lists/*
