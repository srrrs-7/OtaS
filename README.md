# OtaS

AI エージェントにすべての作業を依頼する、Linux ベースの OS です。処理の単位はエージェントのタスクで、タスクはそれぞれサンドボックスで動きます。外部に作用する手段はブローカーを通る呼び出しだけで、呼び出しはすべてポリシーで判定され、監査ログに記録されます。

> **状態**：MVP（中核ライブラリ）を開発中です。まだ OS としては動きません。

## ドキュメント

- [調査 #001 既存OSがAIに最適化されていない点](docs/research/001-ai-os-optimization-perspectives.md)
- [調査 #002 ファイル管理](docs/research/002-file-management.md)
- [設計 #001 モジュール構成・マルチエージェント・通知](docs/design/001-architecture.md)
- [図解](docs/diagrams/index.html)（ブラウザで開く）

## クレート構成

| クレート | 役割 |
|---|---|
| `otas-types` | 共通の型（ID・ラベル・権限・操作・判定・監査記録） |
| `otas-ports` | trait 定義（ポリシー・監査ログ・ツール・モデル） |
| `otas-ifc` | 情報フロー制御（ラベルの伝播と Rule of Two） |
| `otas-policy` | 標準ポリシー（保証 → 権限 → 情報フロー制御の順に判定） |
| `otas-audit` | 監査ログ（追記専用・ハッシュチェーン） |
| `otas-broker` | ブローカー（判定・実行前の記録・実行・ラベル伝播・承認） |
| `otas-ipc` | IPC 層（Cap'n Proto RPC）。タスクごとの窓口と承認の窓口をケイパビリティとして公開する |
| `otas-testkit` | テスト用の偽のツール・モデルと、標準構成のブローカー（公開しない） |
| `otas-adapter-linux` | Linux のサンドボックス（Landlock・seccomp・資源の上限）。起動役 `otas-sandbox-exec` を含む |
| `otasd` | デーモン `otasd`、CLI `otas`、動作確認用のエージェント `otas-demo-agent` |

## 動きの概要

```text
人間 ── otas（CLI）── console.sock ──▶ otasd ─┬─ ブローカー（判定・監査ログ）
                                                └─ 監督 ── サンドボックス ── エージェント
                                                             （stdin がタスク専用の窓口）
```

1. 人間が `otas submit` でタスクを依頼する（プログラム・引数・権限）。
2. `otasd` はタスクを作り、エージェントをサンドボックスで起動して、そのタスク専用の窓口を stdin として渡す。見えるファイルは権限（`read` / `write`）から自動で決まる。
3. エージェントの操作はすべて窓口（ブローカー）を通り、判定されて監査ログに残る。高リスクの操作は承認待ちになる。
4. 人間が `otas pending` で確認し、`otas approve` / `otas reject` で決める。

設定の例は [config/otasd.example.toml](config/otasd.example.toml)、依頼から承認までの通しの流れは `crates/otasd/tests/e2e.rs` を参照してください。

## 開発

ビルドとテストは、すべてコンテナの中で行います。ホストのディレクトリはマウントせず、ソースはイメージのビルド時にコピーします。

```sh
# テスト（ソースを変更したら --build で取り込み直す）
docker compose run --build --rm dev

# 静的解析と整形チェック
docker compose run --build --rm dev cargo clippy --workspace --all-targets -- -D warnings
docker compose run --build --rm dev cargo fmt --all --check
```

`docker compose` が使えない環境では `docker-compose` を使ってください。

コンテナで作ったファイル（整形結果や `Cargo.lock`）をホストに戻すときは、名前付きのコンテナで実行してから `docker cp` で取り出します。

```sh
docker compose run --build --name otas-sync dev cargo fmt --all
docker cp otas-sync:/workspace/crates/. ./crates/
docker rm otas-sync
```

`--build` のたびに古いイメージが残ります。OtaS のイメージだけを選んで掃除するには、ラベルで絞り込みます（他のプロジェクトのイメージには触れません）。

```sh
docker image prune -f --filter label=org.otas.dev=true
```

## ライセンス

[Apache License 2.0](LICENSE)
