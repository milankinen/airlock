FROM debian:trixie-slim

RUN apt-get update && apt-get install -y git \
      curl build-essential capnproto libcapnp-dev musl-tools nodejs
RUN curl https://mise.run | sh


RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --default-toolchain none
RUN echo 'export PATH=~/.local/bin:~/.cargo/bin:$PATH' >> ~/.bashrc && \
    echo 'eval "$(~/.local/bin/mise activate bash)"' >> ~/.bashrc && \
    echo '[[ -f ~/.bashrc ]] && source ~/.bashrc' >> ~/.bash_profile

SHELL ["/bin/bash", "-l", "-c"]

RUN mise use -g node@22

ENV MISE_TRUSTED_CONFIG_PATHS="/"
RUN --mount=type=bind,rw,source=.,target=/workspace \
    cd /workspace && \
    mise install && \
    rustup install && \
    rustup target add aarch64-unknown-linux-musl && \
    rustup component add rust-analyzer

RUN curl -fsSL https://claude.ai/install.sh | bash
RUN curl -fsSL https://gh.io/copilot-install | bash
RUN npm i -g @openai/codex

ENTRYPOINT ["/bin/bash"]

