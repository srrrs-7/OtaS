# OtaS の Makefile。ビルド・テスト・起動はすべてコンテナの中で行う（ホストでは動かさない）。
#
#   make up                 手元でイメージをビルドして otasd を起動する
#   make up IMAGE=ghcr.io/srrrs-7/otas:0.1.1   公開されたイメージで起動する
#   make otas ARGS=tasks    CLI を使う
#   make help               ほかのターゲットの一覧

IMAGE      ?= otas:local
NAME       ?= otasd
VOLUME     ?= otas-state
MEMORY     ?= 2g
PIDS       ?= 256
# 設定を差し替えるときは CONFIG=./otasd.toml（読み取り専用でマウントする）
CONFIG     ?=
# モデルの API キーを渡す環境変数の名前（値はこのシェルの環境から渡し、Makefile やイメージには書かない）
API_KEY_ENV ?= ANTHROPIC_API_KEY

# docker compose が使えなければ docker-compose を使う
COMPOSE := $(shell docker compose version >/dev/null 2>&1 && echo "docker compose" || echo docker-compose)
DEV_LABEL := org.otas.dev=true

.DEFAULT_GOAL := help
.PHONY: help image up down restart logs status otas shell test test-privileged lint fmt clean

help: ## ターゲットの一覧
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

image: ## 配布用イメージを手元でビルドする（IMAGE=otas:local）
	docker build -f packaging/docker/Dockerfile -t $(IMAGE) --label $(DEV_LABEL) .

# 手元でビルドしたイメージ（otas:local）なら先にビルドし、公開されたイメージなら pull する
up: $(if $(filter otas:local,$(IMAGE)),image) ## otasd を起動する（権限なし・ルートは読み取り専用）
	@docker rm -f $(NAME) >/dev/null 2>&1 || true
	docker run -d --name $(NAME) --label $(DEV_LABEL) \
		--cap-drop ALL --security-opt no-new-privileges \
		--read-only --tmpfs /tmp \
		--memory $(MEMORY) --pids-limit $(PIDS) \
		--restart unless-stopped \
		-e $(API_KEY_ENV) \
		-v $(VOLUME):/var/lib/otas \
		$(if $(CONFIG),-v $(abspath $(CONFIG)):/etc/otas/otasd.toml:ro) \
		$(IMAGE)
	@for i in 1 2 3 4 5 6 7 8 9 10; do \
		docker exec $(NAME) test -S /var/lib/otas/console.sock 2>/dev/null && { echo "otasd を起動しました（make otas ARGS=tasks で使えます）"; exit 0; }; \
		sleep 1; \
	done; echo "otasd が起動しません。make logs で確認してください"; exit 1

down: ## otasd を止めて消す（状態のボリュームは残す）
	docker rm -f $(NAME)

restart: down up ## otasd を起動し直す

logs: ## otasd のログを見る
	docker logs -f $(NAME)

status: ## otasd の状態とタスクの一覧
	@docker ps --filter name=^/$(NAME)$$ --format 'table {{.Names}}\t{{.Image}}\t{{.Status}}'
	@docker exec $(NAME) otas tasks

otas: ## CLI を使う（例: make otas ARGS="diff <タスクの ID>"）
	docker exec -i $(NAME) otas $(ARGS)

shell: ## otasd のコンテナでシェルを開く（一般ユーザー）
	docker exec -it $(NAME) sh

test: ## テスト（特権なしのコンテナ）
	$(COMPOSE) run --build --rm dev

test-privileged: ## 名前空間・OverlayFS・Btrfs・cgroups のテスト（特権のコンテナ）
	$(COMPOSE) run --build --rm dev-privileged

lint: ## clippy と fmt のチェック
	$(COMPOSE) run --build --rm dev cargo clippy --workspace --all-targets -- -D warnings
	$(COMPOSE) run --rm dev cargo fmt --all --check

clean: ## OtaS の開発用イメージとコンテナを片付ける（他のプロジェクトには触れない。状態のボリュームは残す）
	-docker rm -f $(NAME)
	docker container prune -f --filter label=$(DEV_LABEL)
	docker image prune -f --filter label=$(DEV_LABEL)
