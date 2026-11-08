# OtaS 開発用コンテナ
# ホストではビルドもテストも実行しない。ソースはビルド時に COPY し、ホストのディレクトリはマウントしない。
FROM rust:1.99-slim-bookworm

# OtaS のイメージだけを選んで掃除できるようにする:
#   docker image prune -f --filter label=org.otas.dev=true
LABEL org.otas.dev="true"

RUN apt-get update \
    && apt-get install -y --no-install-recommends capnproto pkg-config ca-certificates git btrfs-progs \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add clippy rustfmt

# root ではなく一般ユーザーで作業する
ARG UID=10001
RUN useradd --create-home --uid "${UID}" dev \
    && mkdir -p /workspace /cache/cargo /cache/target \
    && chown -R dev:dev /workspace /cache

USER dev
# 依存クレートとビルド成果物は名前付きボリューム（/cache）に置き、再ビルドを速くする
ENV CARGO_HOME=/cache/cargo \
    CARGO_TARGET_DIR=/cache/target \
    PATH=/usr/local/cargo/bin:$PATH

WORKDIR /workspace
COPY --chown=dev:dev . .

CMD ["cargo", "test", "--workspace"]
