---
name: update-crates
description: OtaS の Rust の依存 crate を更新する手順。semver の範囲内の更新（cargo update）と、メジャー版の更新（Cargo.toml の書き換えとコードの修正）を、コンテナの中だけでビルド・テストしながら安全に行い、cargo audit で既知の脆弱性も確かめる。「crate を更新して」「依存を上げて」「cargo update して」「〇〇 を新しい版にして」「脆弱性のある crate を直して」と頼まれたときに使う。
---

# 依存 crate の更新

OtaS の依存 crate を更新する。**ビルド・テスト・cargo の実行はすべて compose の dev コンテナの中で行い、ホストでは動かさない。ホストのディレクトリはマウントしない。**

## 守ること

- **ホストで cargo を動かさない。** 変更したファイルはコンテナの中で作り、`docker cp` で取り出す（下の「同期」）。
- **公開から 14 日たっていない版は入れない**（サプライチェーン攻撃の多くは公開直後に見つかって取り下げられるため）。yanked の版も入れない。
- **MSRV（`Cargo.toml` の `rust-version`）を勝手に上げない。** 新しい版が MSRV を上げる場合は、上げてよいかユーザーに聞く。
- **1回のコミットで上げるメジャー版は 1 crate だけ。** semver の範囲内の更新（`cargo update`）はまとめて 1 コミットでよい。
- **テストが通らない更新はコミットしない。** 直せなければ、その crate は元に戻して理由を報告する。
- **push・タグ・PR はユーザーに確認してから。** このスキルはローカルのコミットまで。
- `docs/research/01-ai-os-perspectives.md` はユーザーのファイルなのでコミットに含めない。
- Docker の片付けは `--filter label=org.otas.dev=true` を付けたものだけ（同じ VM に他のプロジェクトがある）。

## 手順

### 0. 準備

```sh
git status --short          # 未コミットの変更がないこと（あればユーザーに聞く）
git switch -c chore/update-crates-$(date +%Y%m%d)
docker-compose build -q dev  # ソースを取り込み直す（docker compose が使えればそれでもよい）
```

### 1. 更新できるものを調べる

```sh
docker-compose run --rm dev cargo update --dry-run --verbose
```

- `Updating a vX -> vY`：semver の範囲内の更新（手順 2）
- `Unchanged a vX (available: vZ)`：メジャー版の更新がある（手順 3）
- `requires Rust 1.NN`：MSRV を上げないと入らない（ユーザーに聞く）

公開日と yanked は crates.io の API で確かめる。これは読み取りだけなのでホストで実行してよい（User-Agent には一般的な名前を使い、メールアドレスは入れない）。

```sh
curl -s -A "otas-update-crates" https://crates.io/api/v1/crates/<crate>/<版> \
  | python3 -I -c 'import json,sys; v=json.load(sys.stdin)["version"]; print(v["num"], v["created_at"], "yanked" if v["yanked"] else "ok")'
```

### 2. semver の範囲内の更新

14 日たっていない版があれば、その crate は `cargo update -p <crate> --precise <前の版>` で止める。

```sh
docker rm -f otas-sync >/dev/null 2>&1
docker-compose run --name otas-sync dev sh -c '
  cargo update &&
  cargo build --workspace --all-targets &&
  cargo test --workspace'
docker cp otas-sync:/workspace/Cargo.lock ./Cargo.lock && docker rm otas-sync
```

### 3. メジャー版の更新（1 crate ずつ）

1. 変更点を読む：crate の CHANGELOG か GitHub のリリースノート（破壊的変更・MSRV・features の変更）。
2. ホストで `Cargo.toml`（ワークスペースの `[workspace.dependencies]` か、各 crate の `[dependencies]`）の版を書き換える。
3. `docker-compose build -q dev` で取り込み、コンテナの中でビルドし、エラーをホストのソースで直す。これを通るまで繰り返す。
4. `Cargo.lock` を同期で取り出す。

### 4. 確かめる（すべてコンテナの中）

```sh
docker-compose build -q dev
docker-compose run --rm dev cargo test --workspace
docker-compose run --rm dev-privileged     # 名前空間・OverlayFS・Btrfs・cgroups のテスト
docker-compose run --rm dev cargo clippy --workspace --all-targets -- -D warnings
docker-compose run --rm dev cargo fmt --all --check
```

整形が必要なら、コンテナの中で `cargo fmt --all` をしてから `crates/` を同期で取り出す。

### 5. 既知の脆弱性（cargo audit）

`cargo-audit` は dev コンテナのキャッシュのボリューム（`/cache/cargo/bin`）に一度だけ入れる。

```sh
docker-compose run --rm dev sh -c '
  command -v cargo-audit >/dev/null || cargo install cargo-audit --locked
  cargo audit'
```

脆弱性が出たら、直る版に上げる（手順 2 か 3）。上げられないものは、内容と影響を報告する（勝手に `ignore` しない）。

### 6. 同期とコミット

コンテナの中で変わったファイルは、名前付きのコンテナから取り出す。

```sh
docker rm -f otas-sync >/dev/null 2>&1
docker-compose run --name otas-sync dev sh -c '<コンテナの中での作業>'
docker cp otas-sync:/workspace/crates/. ./crates/
docker cp otas-sync:/workspace/Cargo.lock ./Cargo.lock
docker rm otas-sync
```

コミットメッセージは日本語で、何をどの版からどの版にしたかを書く。最後にリポジトリの決まりの `Co-Authored-By` の行を付ける。

```text
依存 crate を更新する（semver の範囲内）

- tokio 1.48.0 → 1.49.0
- serde 1.0.228 → 1.0.230
```

### 7. 報告

日本語で、表にまとめて報告する。

| crate | 前 | 後 | 種類 | 結果 |
|---|---|---|---|---|
| sha2 | 0.10.9 | 0.11.0 | メジャー | 更新（API の変更に対応） |
| uuid | 1.26.1 | — | — | 見送り（MSRV 1.89 が必要） |

あわせて、テストの件数（通常・特権）、clippy と fmt の結果、cargo audit の結果、見送ったものとその理由、push してよいかの確認を書く。
