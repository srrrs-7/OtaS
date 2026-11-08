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
