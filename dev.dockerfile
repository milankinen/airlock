FROM debian:trixie-slim

RUN apt-get update && apt-get install -y git \
      curl build-essential capnproto libcapnp-dev musl-tools
RUN curl https://mise.run | sh
RUN curl -fsSL https://claude.ai/install.sh | bash
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --default-toolchain none
RUN echo 'export PATH=~/.local/bin:~/.cargo/bin:$PATH' >> .bashrc
RUN echo 'eval "$(~/.local/bin/mise activate bash)"' >> ~/.bashrc

SHELL ["/bin/bash", "-l", "-c"]
ENV MISE_TRUSTED_CONFIG_PATHS="/"
RUN --mount=type=bind,rw,source=.,target=/workspace \
    cd /workspace && \
    mise install && \
    rustup install && \
    rustup target add aarch64-unknown-linux-musl && \
    rustup component add rust-analyzer

ENTRYPOINT ["/bin/bash"]
