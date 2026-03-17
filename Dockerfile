FROM node:22-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g @anthropic-ai/claude-code

WORKDIR /app
COPY claude-remote-chat /app/claude-remote-chat

RUN useradd -r -m appuser \
    && mkdir -p /data /projects \
    && chown appuser:appuser /data /projects /app \
    && chmod +x /app/claude-remote-chat

USER appuser
ENTRYPOINT ["/app/claude-remote-chat"]
