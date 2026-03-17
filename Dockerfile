FROM node:22-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g @anthropic-ai/claude-code

WORKDIR /app
COPY claude-remote-chat /app/claude-remote-chat

RUN mkdir -p /data /projects \
    && chmod +x /app/claude-remote-chat

ENTRYPOINT ["/app/claude-remote-chat"]
