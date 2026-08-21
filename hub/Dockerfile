# BRVG hub — multi-arch (amd64/arm64/armv7 via buildx) Node image. Two stages so the runtime
# carries no toolchain and no dev deps: the hub has zero runtime dependencies (Node built-in
# http + global fetch), so `npm ci --omit=dev` installs nothing and the final image is just Node +
# the compiled dist.

FROM node:22-alpine AS build
WORKDIR /app
COPY package.json tsconfig.json ./
RUN npm install --no-audit --no-fund
COPY src ./src
RUN npm run build

FROM node:22-alpine
WORKDIR /app
ENV NODE_ENV=production
COPY --from=build /app/dist ./dist
COPY package.json ./
# No runtime deps to install. Run unprivileged.
USER node
EXPOSE 8181
# Config comes from the environment (see README): VID, DEVICE_ID, DEVICE_TOKEN, WORKER_URL.
CMD ["node", "dist/index.js"]
