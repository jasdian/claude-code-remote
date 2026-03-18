FROM rust:1-bookworm AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY migrations/ migrations/
RUN cargo build --release && strip target/release/claude-crew

# ── Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates git curl \
    && rm -rf /var/lib/apt/lists/*

# Create user first so the Claude install lands in its home dir
RUN useradd -r -m appuser \
    && mkdir -p /data /projects \
    && chown appuser:appuser /data /projects

# Install Claude Code CLI for appuser
USER appuser
RUN curl -fsSL https://claude.ai/install.sh | bash
ENV PATH="/home/appuser/.local/bin:${PATH}"

WORKDIR /app
COPY --from=builder --chown=appuser:appuser /src/target/release/claude-crew /app/claude-crew

ENTRYPOINT ["/app/claude-crew"]
