FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates git curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY claude-crew /app/claude-crew

RUN useradd -r -m appuser \
    && mkdir -p /data /projects \
    && chown appuser:appuser /data /projects /app \
    && chmod +x /app/claude-crew

USER appuser

# Install Claude Code as standalone binary (not via npm)
RUN curl -fsSL https://claude.ai/install.sh | bash

ENV PATH="/home/appuser/.local/bin:${PATH}"

ENTRYPOINT ["/app/claude-crew"]
