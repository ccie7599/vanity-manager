.PHONY: build run run-env clean provision seed stats drain deploy ew-build ew-deploy-staging ew-deploy-prod test-smoke help

# Default: show targets
help:
	@awk 'BEGIN{FS=":.*?## "} /^[a-zA-Z_-]+:.*?## / {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

# ---------------------------------------------------------------------------
# Provisioning
# ---------------------------------------------------------------------------
provision: ## Provision Linode Object Storage bucket + Akamai EdgeKV namespace (idempotent)
	@scripts/provision.sh

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
build: ## Build all Spin components
	spin build

clean: ## Clean build artifacts + local Spin state
	rm -rf target .spin-state

# ---------------------------------------------------------------------------
# Local dev
# ---------------------------------------------------------------------------
run: ## Run locally with dev token, no S3/EKV (local-only sync path)
	spin up --state-dir .spin-state --variable admin_token=dev-token-changeme

run-env: ## Run locally with provision.env loaded (S3 + EKV wired)
	@test -f provision.env || { echo "provision.env missing — run \`make provision\` first"; exit 1; }
	@set -a; . ./provision.env; set +a; \
	spin up --state-dir .spin-state \
	  --variable admin_token=dev-token-changeme \
	  --variable s3_endpoint="$$S3_ENDPOINT" \
	  --variable s3_region="$$S3_REGION" \
	  --variable s3_bucket="$$S3_BUCKET" \
	  --variable s3_access_key="$$S3_ACCESS_KEY" \
	  --variable s3_secret_key="$$S3_SECRET_KEY" \
	  --variable akamai_host="$$AKAMAI_HOST" \
	  --variable akamai_client_token="$$AKAMAI_CLIENT_TOKEN" \
	  --variable akamai_client_secret="$$AKAMAI_CLIENT_SECRET" \
	  --variable akamai_access_token="$$AKAMAI_ACCESS_TOKEN" \
	  --variable ekv_network="$$EKV_NETWORK" \
	  --variable ekv_namespace="$$EKV_NAMESPACE" \
	  --variable ekv_group="$$EKV_GROUP"

# ---------------------------------------------------------------------------
# Admin operations (against local dev server)
# ---------------------------------------------------------------------------
seed: ## Seed sample data via /v1/import
	@curl -s -X POST "http://127.0.0.1:3000/api/v1/import?token=dev-token-changeme" \
	  -H "content-type: application/json" \
	  -d @sample-data/redirects.json | python3 -m json.tool

stats: ## Fetch /v1/stats
	@curl -s "http://127.0.0.1:3000/api/v1/stats?token=dev-token-changeme" | python3 -m json.tool

drain: ## Fire reconcile drain
	@curl -s -X POST http://127.0.0.1:3000/_reconcile/drain | python3 -m json.tool

test-smoke: ## Run the smoke-test script end-to-end
	@bash scripts/smoke-test.sh

# ---------------------------------------------------------------------------
# Deploy
# ---------------------------------------------------------------------------
APP_NAME ?= vanity-manager

deploy: build ## Deploy Spin app to Akamai Functions
	@test -f provision.env || { echo "provision.env missing — run \`make provision\` first"; exit 1; }
	@set -a; . ./provision.env; set +a; \
	if [ -f .spin-aka/app-id ] || [ -n "$$APP_ID" ]; then CREATE_FLAG=""; else CREATE_FLAG="--create-name $(APP_NAME)"; fi; \
	spin aka deploy $$CREATE_FLAG --no-confirm \
	  --variable admin_token="$${ADMIN_TOKEN:-$$(openssl rand -hex 32)}" \
	  --variable s3_endpoint="$$S3_ENDPOINT" \
	  --variable s3_region="$$S3_REGION" \
	  --variable s3_bucket="$$S3_BUCKET" \
	  --variable s3_access_key="$$S3_ACCESS_KEY" \
	  --variable s3_secret_key="$$S3_SECRET_KEY" \
	  --variable akamai_host="$$AKAMAI_HOST" \
	  --variable akamai_client_token="$$AKAMAI_CLIENT_TOKEN" \
	  --variable akamai_client_secret="$$AKAMAI_CLIENT_SECRET" \
	  --variable akamai_access_token="$$AKAMAI_ACCESS_TOKEN" \
	  --variable ekv_network="$$EKV_NETWORK" \
	  --variable ekv_namespace="$$EKV_NAMESPACE" \
	  --variable ekv_group="$$EKV_GROUP"

# ---------------------------------------------------------------------------
# EdgeWorker deploy (version = git describe)
# ---------------------------------------------------------------------------
EW_VERSION := $(shell git describe --always --dirty 2>/dev/null || echo dev)
EW_NAME ?= vanity-manager
EW_NETWORK ?= STAGING

ew-build: ## Build the EdgeWorker bundle (./edgeworker → dist/vanity-manager-<ver>.tgz)
	@bash scripts/ew-build.sh $(EW_VERSION)

ew-deploy-staging: ew-build ## Upload + activate EW on staging network
	@bash scripts/ew-deploy.sh $(EW_NAME) $(EW_VERSION) STAGING

ew-deploy-prod: ew-build ## Promote current EW version to production
	@bash scripts/ew-deploy.sh $(EW_NAME) $(EW_VERSION) PRODUCTION
