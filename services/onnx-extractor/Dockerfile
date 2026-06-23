# SPDX-License-Identifier: MIT
#
# The GLiNER entity-extraction sidecar for pg_graphwright. graphwright and graphwright-onnx are
# installed from git (they build their dist on npm's prepare), and @lmoe/gliner-onnx pulls the
# onnxruntime backend. The GLiNER model is fetched on demand at startup, not bundled.
FROM node:20-slim

# git: graphwright/graphwright-onnx are git dependencies. ca-certificates: model + package fetches.
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY package.json ./
RUN npm install --no-audit --no-fund
COPY server.mjs ./

ENV PORT=8081
EXPOSE 8081
CMD ["node", "server.mjs"]
