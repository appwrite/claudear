ARG APP_VERSION=0.1.0
ARG RUST_VERSION=1.93
ARG BUN_VERSION=1.3
ARG DEBIAN_VERSION=trixie
ARG ONNXRUNTIME_VERSION=1.24.2
ARG VECTORLITE_VERSION=16a01af79add
ARG MCP_GRAFANA_VERSION=v1.3.0

ARG GIT_USER_NAME="Claudear"
ARG GIT_USER_EMAIL="claudear@noreply.local"

ARG CLAUDEAR_SENTRY_DSN=""
ARG CLAUDEAR_SENTRY_ENVIRONMENT="production"
ARG CLAUDEAR_SENTRY_RELEASE="claudear-dashboard@${APP_VERSION}"

FROM oven/bun:${BUN_VERSION} AS dashboard
ARG CLAUDEAR_SENTRY_DSN
ARG CLAUDEAR_SENTRY_ENVIRONMENT
ARG CLAUDEAR_SENTRY_RELEASE
ENV CLAUDEAR_SENTRY_DSN=${CLAUDEAR_SENTRY_DSN}
ENV CLAUDEAR_SENTRY_ENVIRONMENT=${CLAUDEAR_SENTRY_ENVIRONMENT}
ENV CLAUDEAR_SENTRY_RELEASE=${CLAUDEAR_SENTRY_RELEASE}

WORKDIR /app/dashboard
COPY dashboard/package.json dashboard/bun.lock* ./
RUN bun install --frozen-lockfile
COPY dashboard/ ./
RUN bun run build

FROM debian:${DEBIAN_VERSION}-slim AS vectorlite
ARG VECTORLITE_VERSION

RUN apt-get update && apt-get install -y \
    build-essential \
    cmake \
    curl \
    git \
    ninja-build \
    pkg-config \
    python3 \
    zip \
    unzip \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

RUN git clone --recurse-submodules https://github.com/1yefuwang1/vectorlite.git . \
    && git checkout "${VECTORLITE_VERSION}"

ENV CMAKE_POLICY_VERSION_MINIMUM=3.5

RUN python3 bootstrap_vcpkg.py

RUN cmake --preset release && cmake --build build/release -j$(nproc)

FROM debian:${DEBIAN_VERSION}-slim AS claude
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN useradd -m -u 1000 appuser
USER appuser
RUN curl -fsSL https://claude.ai/install.sh | bash

FROM rust:${RUST_VERSION}-slim-${DEBIAN_VERSION} AS builder
ARG APP_VERSION

WORKDIR /app

RUN apt-get update && apt-get install -y \
    build-essential \
    cmake \
    libclang-dev \
    libssl-dev \
    pkg-config \
    perl \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock build.rs ./
COPY crates/claudear-core/Cargo.toml crates/claudear-core/Cargo.toml
COPY crates/claudear-config/Cargo.toml crates/claudear-config/Cargo.toml
COPY crates/claudear-storage/Cargo.toml crates/claudear-storage/Cargo.toml
COPY crates/claudear-analysis/Cargo.toml crates/claudear-analysis/Cargo.toml
COPY crates/claudear-integrations/Cargo.toml crates/claudear-integrations/Cargo.toml
COPY crates/claudear-engine/Cargo.toml crates/claudear-engine/Cargo.toml
COPY crates/claudear-e2e/Cargo.toml crates/claudear-e2e/Cargo.toml
RUN if [ -n "${APP_VERSION}" ] && [ "${APP_VERSION}" != "0.1.0" ]; then \
      sed -i '/^\[package\]/,/^$/ s/^version = .*/version = "'"${APP_VERSION}"'"/' Cargo.toml; \
    fi
# Create stubs for each crate so cargo can resolve the workspace and cache dependencies
RUN mkdir -p src crates/claudear-core/src crates/claudear-config/src \
    crates/claudear-storage/src crates/claudear-analysis/src \
    crates/claudear-integrations/src crates/claudear-engine/src \
    crates/claudear-e2e/src \
    && echo "fn main() {}" > src/main.rs \
    && echo "" > src/lib.rs \
    && for c in core config storage analysis integrations engine; do \
         echo "" > crates/claudear-$c/src/lib.rs; done \
    && echo "fn main() {}" > crates/claudear-e2e/src/main.rs
RUN mkdir -p dashboard/dist
RUN cargo build --release --bin claudear && rm -rf src crates/*/src

COPY src ./src
COPY crates ./crates
COPY migrations ./migrations

COPY --from=dashboard /app/dashboard/dist ./dashboard/dist
RUN touch src/main.rs src/lib.rs \
    && for c in core config storage analysis integrations engine; do touch crates/claudear-$c/src/lib.rs; done \
    && cargo build --release --bin claudear

FROM debian:${DEBIAN_VERSION}-slim AS final
ARG GIT_USER_NAME
ARG GIT_USER_EMAIL
ARG MCP_GRAFANA_VERSION

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    git \
    jq \
    libgomp1 \
    openssh-client \
    sqlite3 \
    && rm -rf /var/lib/apt/lists/* /usr/share/doc/* /usr/share/man/* /usr/share/locale/* \
    && ARCH=$(dpkg --print-architecture) \
    && curl -fsSL "https://cli.github.com/packages/githubcli-archive-keyring.gpg" \
       -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=${ARCH} signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
       > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*

# Grafana MCP server, so agents can query Prometheus metrics and Loki logs while
# triaging. A single static Go binary: no extra language runtime, and nothing is
# downloaded on first use the way `uvx mcp-grafana` would. Attached to runs only
# when [agent.providers.claude.mcp.grafana] is configured.
RUN ARCH=$(dpkg --print-architecture) \
    && case "${ARCH}" in \
         amd64) MCP_ARCH=x86_64 ;; \
         arm64) MCP_ARCH=arm64 ;; \
         *) echo "unsupported arch: ${ARCH}" >&2; exit 1 ;; \
       esac \
    && curl -fsSL "https://github.com/grafana/mcp-grafana/releases/download/${MCP_GRAFANA_VERSION}/mcp-grafana_Linux_${MCP_ARCH}.tar.gz" \
       | tar -xz -C /usr/local/bin mcp-grafana \
    && chmod 755 /usr/local/bin/mcp-grafana

COPY --from=vectorlite /build/build/release/vectorlite/vectorlite.so /usr/local/lib/vectorlite.so
COPY --from=builder /app/target/release/claudear /usr/local/bin/claudear
COPY --chmod=755 docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

RUN adduser --disabled-password --uid 1000 --gecos "" appuser \
    && mkdir -p /app/data /app/repos /app/workspace /home/appuser/.cache/fastembed /home/appuser/.claude \
    && chown -R appuser:appuser /app /home/appuser/.cache /home/appuser/.claude

COPY --from=claude --chown=appuser:appuser /home/appuser/.local /home/appuser/.local

USER appuser

RUN git config --global user.name "${GIT_USER_NAME}" \
    && git config --global user.email "${GIT_USER_EMAIL}" \
    && git config --global init.defaultBranch main

ENV PATH="/home/appuser/.local/bin:${PATH}"

ENV WORKSPACE=/app/workspace
ENV DATA_DIR=/app/data
ENV REPOS_DIR=/app/repos
ENV EMBEDDING_CACHE_DIR=/home/appuser/.cache/fastembed

# Claude Code authentication (provide at runtime):
#   Option 1: Set ANTHROPIC_API_KEY env var (API key)
#   Option 2: Omit ANTHROPIC_API_KEY and the entrypoint will run 'claude auth login'
#             (prints a URL to open in your browser for OAuth)

EXPOSE 3100 443 80

HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:3100/api/health || exit 1

ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["claudear"]
