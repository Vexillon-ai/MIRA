# Tiny deterministic OpenAI-compatible mock LLM for the Restricted Mode e2e suite.
FROM node:20-bookworm-slim
WORKDIR /app
COPY server.js ./
EXPOSE 8080
CMD ["node", "server.js"]
