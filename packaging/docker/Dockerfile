# OtaS の配布用イメージ（otasd をサービスとして動かす）。開発用の ./Dockerfile とは別。
#
#   docker build -f packaging/docker/Dockerfile -t otas:local .
#
# 既定のサンドボックスは landlock（特権不要）。ホストのカーネルは 6.7 以降（Landlock ABI v4）が必要。
# 使えない環境では、制限なしでは動かさずに起動を拒否する。

ARG RUST_VERSION=1.99

# ---- ビルド ----
FROM rust:${RUST_VERSION}-slim-bookworm AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends capnproto pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# BuildKit のない Docker でもビルドできるよう、キャッシュのマウントは使わない（CI はレイヤーのキャッシュを使う）
RUN cargo build --release --locked -p otasd -p otas-adapter-linux --bins \
    && mkdir /out \
    && for bin in otasd otas otas-agent otas-sandbox-exec; do cp "target/release/$bin" /out/; done

# ---- 実行 ----
# タスクは /bin/sh などのシステムのプログラムを使うので、distroless ではなく slim にする
FROM debian:bookworm-slim

# tini：PID 1 としてシグナル（docker stop の SIGTERM）を otasd に渡し、孤児を回収する
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/*

# root ではなく専用のユーザーで動かす
ARG UID=10001
RUN useradd --system --uid "${UID}" --user-group --home-dir /var/lib/otas --shell /usr/sbin/nologin otas \
    && install -d -m 0700 -o otas -g otas /var/lib/otas \
    && install -d -m 0755 /etc/otas /usr/local/libexec

COPY --from=build /out/otasd /out/otas /out/otas-agent /usr/local/bin/
COPY --from=build /out/otas-sandbox-exec /usr/local/libexec/otas-sandbox-exec
COPY packaging/docker/otasd.toml /etc/otas/otasd.toml
COPY LICENSE /usr/share/doc/otas/LICENSE

LABEL org.opencontainers.image.title="OtaS" \
      org.opencontainers.image.description="AI エージェントに最適化した OS のデーモン（otasd）" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.source="https://github.com/srrrs-7/OtaS"

# CLI（docker exec <コンテナ> otas tasks）が使うソケット
ENV OTAS_SOCKET=/var/lib/otas/console.sock
USER otas
WORKDIR /var/lib/otas
VOLUME ["/var/lib/otas"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s \
    CMD ["otas", "versions"]

ENTRYPOINT ["tini", "--", "otasd", "--config", "/etc/otas/otasd.toml"]
