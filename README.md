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
| `otas-adapter-linux` | Linux のサンドボックス（Landlock・seccomp・資源の上限。root で動かす場合は名前空間（PID を含む）と pivot_root、cgroups v2 によるメモリ・プロセス数・CPU の上限も。一般ユーザーのデーモンでは、ユーザー名前空間による root なしの方式も）。起動役 `otas-sandbox-exec` を含む |
| `otas-workspace` | 作業領域と正本の版管理。作業領域はコピーか OverlayFS、版はハードリンクか Btrfs の読み取り専用スナップショット。3者比較によるコミット、巻き戻し、ファイルを通じたラベルの伝播 |
| `otas-models` | モデルのアダプタ。Claude API（Messages API）と OpenAI 互換 API（Ollama・llama.cpp などのローカルモデルを含む）。API キーはデーモンの中だけに置く |
| `otas-state` | 状態の保存（ファイル・メモリ）。ブローカーと監督が再起動をまたいで続きから動けるようにする |
| `otasd` | デーモン `otasd`、CLI `otas`、動作確認用のエージェント `otas-demo-agent` |

## 動きの概要

```text
人間 ── otas（CLI）── console.sock ──▶ otasd ─┬─ ブローカー（判定・監査ログ）
                                                └─ 監督 ── サンドボックス ── エージェント
                                                             （stdin がタスク専用の窓口）
```

1. 人間が `otas submit` でタスクを依頼する（プログラム・引数・権限）。
2. `otasd` はタスクを作り、権限（`read` / `write`）の範囲だけを見せる作業領域を用意する。見せたデータのラベル（私的など）はタスクに取り込まれる。
3. エージェントをサンドボックスで起動し、そのタスク専用の窓口を stdin として、作業領域の場所を `OTAS_DATA` として渡す。
4. エージェントの操作はすべて窓口（ブローカー）を通り、判定されて監査ログに残る。高リスクの操作は承認待ちになる（`otas pending` / `approve` / `reject`）。
5. エージェントが書いたファイルは作業領域にたまり、正本にはまだ反映されない。人間が `otas diff` で確認し、`otas commit` か `otas discard` で決める。コミットしたファイルにはタスクのラベルが付く。
6. 正本は版として残り、`otas versions` / `otas rollback` で戻せる。

`otas` を引数なしで（または `otas chat` で）起動すると対話モードになります。入力した依頼は標準のエージェント（`otas-agent`）のタスクとして実行され、経過がその場で表示されます。処理は自動で進み、人間に確かめるのは、ブローカーが承認待ちにした操作と、変更のコミットだけです。承認するとエージェントはその結果を受け取って続きを進めます。モデルは `--model` か環境変数 `OTAS_MODEL`、権限は `--read` / `--write` / `--tool` で指定します（`--read` も `--write` もなければ `/data` に書けます）。

設定の例は [config/otasd.example.toml](config/otasd.example.toml)、依頼から承認までの通しの流れは `crates/otasd/tests/e2e.rs` を参照してください。

## Docker イメージで動かす

`make` で起動できます（`make help` でターゲットの一覧）。

```sh
make up                                     # 手元でイメージをビルドして otasd を起動
make up IMAGE=ghcr.io/srrrs-7/otas:0.1.3    # 公開されたイメージで起動
make chat MODEL=claude                      # 対話モード（自然言語で依頼する。設定に [[models]] が必要）
make otas ARGS=tasks                        # CLI（submit・diff・commit なども同じ）
make logs / make status / make down
make test / make test-privileged / make lint
```

手で起動する場合は次のとおりです。

`otasd` を Docker のサービスとして動かせます。サンドボックスは `landlock`（特権不要）で、ホストのカーネルは 6.7 以降が必要です（Landlock ABI v4。満たさなければ起動を拒否します）。

```sh
docker run -d --name otasd \
  --cap-drop ALL --security-opt no-new-privileges --read-only --tmpfs /tmp \
  --memory 2g --pids-limit 256 \
  -v otas-state:/var/lib/otas \
  ghcr.io/srrrs-7/otas:0.1.3

docker exec otasd otas tasks          # CLI は同じコンテナの中で使う
```

- 設定は `/etc/otas/otasd.toml`（既定は [packaging/docker/otasd.toml](packaging/docker/otasd.toml)）。差し替えるときは読み取り専用でマウントします。
- API キーはイメージや設定に入れず、環境変数か Docker の secrets（`api_key_file`）で渡します。
- タスク全体の資源の上限は、コンテナの上限（`--memory`・`--pids-limit`）でかけます。
- `namespace`（`--privileged` が必要）と `rootless`（ユーザー名前空間が必要）は、Docker の既定では使えません。
- compose の例は [packaging/docker/compose.yml](packaging/docker/compose.yml)。

イメージのビルド：`docker build -f packaging/docker/Dockerfile -t otas:local .`

公開は `v<版>` のタグを push すると GitHub Actions（[.github/workflows/release.yml](.github/workflows/release.yml)）が行います。テスト → 脆弱性スキャン → amd64・arm64 のビルド → GHCR への push → SBOM・来歴の証明・cosign の署名、の順です。

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

名前空間・OverlayFS・Btrfs を使う機能のテストは root と mount が必要なので、特権を足したテスト専用のサービスで動かします。普段の開発（`dev`）は特権なしのままです。

```sh
docker compose run --build --rm dev-privileged
```

このサービスは `privileged: true` で root として動き、`/tmp` に置いた Btrfs のイメージを loop デバイスでマウントしてから、すべてのテストを実行します（`scripts/test-privileged.sh`）。

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
