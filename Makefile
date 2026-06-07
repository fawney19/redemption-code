API_HOST ?= 127.0.0.1
API_PORT ?= 8318
WEB_HOST ?= 127.0.0.1
WEB_PORT ?= 5178

AETHER_POOL_ADDR ?= $(API_HOST):$(API_PORT)
AETHER_POOL_DATABASE_URL ?= sqlite://data/aether-pool.sqlite3
AETHER_POOL_ADMIN_TOKEN ?= admin123
AETHER_POOL_SECRET_KEY ?= dev-only-secret-key
AETHER_POOL_REDEEM_PROBE_CONCURRENCY ?= 16
VITE_API_BASE ?=
VITE_DEV_API_TARGET ?= http://$(AETHER_POOL_ADDR)

.PHONY: dev dev-api dev-web web-deps require-admin-token

dev: require-admin-token web-deps
	@echo "API:   http://$(AETHER_POOL_ADDR)"
	@echo "Web:   http://$(WEB_HOST):$(WEB_PORT)"
	@echo "Admin: http://$(WEB_HOST):$(WEB_PORT)/act"
	@echo "Admin password: $(AETHER_POOL_ADMIN_TOKEN)"
	@cleanup() { kill $$api_pid $$web_pid 2>/dev/null || true; }; \
	( \
		exec env \
			AETHER_POOL_ADDR="$(AETHER_POOL_ADDR)" \
			AETHER_POOL_DATABASE_URL="$(AETHER_POOL_DATABASE_URL)" \
			AETHER_POOL_ADMIN_TOKEN="$(AETHER_POOL_ADMIN_TOKEN)" \
			AETHER_POOL_SECRET_KEY="$(AETHER_POOL_SECRET_KEY)" \
			AETHER_POOL_REDEEM_PROBE_CONCURRENCY="$(AETHER_POOL_REDEEM_PROBE_CONCURRENCY)" \
			cargo run -p aether-pool-api \
	) & api_pid=$$!; \
	( \
		cd frontend && \
		exec env \
			VITE_API_BASE="$(VITE_API_BASE)" \
			VITE_DEV_API_TARGET="$(VITE_DEV_API_TARGET)" \
			npm run dev -- --host "$(WEB_HOST)" --port "$(WEB_PORT)" \
	) & web_pid=$$!; \
	trap 'cleanup' INT TERM EXIT; \
	while true; do \
		if ! kill -0 $$api_pid 2>/dev/null; then \
			wait $$api_pid; status=$$?; cleanup; wait $$web_pid 2>/dev/null || true; exit $$status; \
		fi; \
		if ! kill -0 $$web_pid 2>/dev/null; then \
			wait $$web_pid; status=$$?; cleanup; wait $$api_pid 2>/dev/null || true; exit $$status; \
		fi; \
		sleep 1; \
	done

dev-api: require-admin-token
	@AETHER_POOL_ADDR="$(AETHER_POOL_ADDR)" \
	AETHER_POOL_DATABASE_URL="$(AETHER_POOL_DATABASE_URL)" \
	AETHER_POOL_ADMIN_TOKEN="$(AETHER_POOL_ADMIN_TOKEN)" \
	AETHER_POOL_SECRET_KEY="$(AETHER_POOL_SECRET_KEY)" \
	AETHER_POOL_REDEEM_PROBE_CONCURRENCY="$(AETHER_POOL_REDEEM_PROBE_CONCURRENCY)" \
	cargo run -p aether-pool-api

dev-web: web-deps
	@cd frontend && \
	VITE_API_BASE="$(VITE_API_BASE)" \
	VITE_DEV_API_TARGET="$(VITE_DEV_API_TARGET)" \
	npm run dev -- --host "$(WEB_HOST)" --port "$(WEB_PORT)"

web-deps: frontend/node_modules/.package-lock.json

frontend/node_modules/.package-lock.json: frontend/package.json frontend/package-lock.json
	@echo "Installing web dependencies..."
	@cd frontend && npm ci

require-admin-token:
	@if [ -z "$(strip $(AETHER_POOL_ADMIN_TOKEN))" ]; then \
		echo "AETHER_POOL_ADMIN_TOKEN is required. Set the admin password in your environment:"; \
		echo "  export AETHER_POOL_ADMIN_TOKEN=your-admin-password"; \
		exit 1; \
	fi
