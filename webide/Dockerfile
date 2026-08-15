# syntax=docker/dockerfile:1.7
FROM node:22-bookworm-slim@sha256:d649c27dae7ba0137b3cef5dd75baa422c08dc3d9e3fc0c23dfb172dc3cc6436 AS extension-builder
WORKDIR /src
COPY extension/package.json extension/package-lock.json ./
RUN npm ci --ignore-scripts
COPY extension/tsconfig.json ./
COPY extension/src ./src
RUN npm run check && npm run build && npm prune --omit=dev

FROM node:22-bookworm-slim@sha256:d649c27dae7ba0137b3cef5dd75baa422c08dc3d9e3fc0c23dfb172dc3cc6436 AS addon-tools-extension-builder
WORKDIR /src
COPY extension-addon-tools/package.json extension-addon-tools/package-lock.json ./
RUN npm ci --ignore-scripts
COPY extension-addon-tools/tsconfig.json ./
COPY extension-addon-tools/src ./src
COPY extension-addon-tools/prompts ./prompts
RUN npm run check && npm run build && npm prune --omit=dev

FROM debian:bookworm-slim@sha256:94c4d598b5987d76c38408657aae7118b101662595bf5eefe478e093a0bed2f6 AS code-server-fetcher
ARG TARGETARCH
ARG CODE_SERVER_VERSION=4.132.0
ARG CODE_SERVER_SHA256_AMD64=a38d26f4cb81f768feddff79e2937fd3f39c83d3da8be3da7225e1087e62e4ed
ARG CODE_SERVER_SHA256_ARM64=ade569a677d1c04ee66ef153382b7e15bf261f955407663c7ddc6b87f9ee29fc
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && case "${TARGETARCH}" in \
        amd64) archive_arch=amd64; expected="${CODE_SERVER_SHA256_AMD64}" ;; \
        arm64) archive_arch=arm64; expected="${CODE_SERVER_SHA256_ARM64}" ;; \
        *) echo "unsupported target architecture: ${TARGETARCH}" >&2; exit 1 ;; \
       esac \
    && curl --fail --location --proto '=https' --tlsv1.2 \
        "https://github.com/coder/code-server/releases/download/v${CODE_SERVER_VERSION}/code-server-${CODE_SERVER_VERSION}-linux-${archive_arch}.tar.gz" \
        --output /tmp/code-server.tar.gz \
    && echo "${expected}  /tmp/code-server.tar.gz" | sha256sum --check --strict \
    && mkdir -p /opt/code-server \
    && tar --extract --gzip --file /tmp/code-server.tar.gz --strip-components=1 --directory /opt/code-server \
    && rm /tmp/code-server.tar.gz

FROM debian:bookworm-slim@sha256:94c4d598b5987d76c38408657aae7118b101662595bf5eefe478e093a0bed2f6
LABEL org.opencontainers.image.title="Jexactyl Web IDE" \
      org.opencontainers.image.description="Hardened code-server runtime managed by Wings" \
      org.opencontainers.image.source="https://github.com/Jexactyl/Jexactyl"
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /opt/jexactyl/extensions-ro/jexactyl.web-ide-1.3.3/dist /opt/jexactyl/extensions-ro/jexactyl-mod-manager-tools-1.0.5/dist /opt/jexactyl/bin
COPY --from=code-server-fetcher /opt/code-server /opt/code-server
COPY --from=extension-builder /src/package.json /opt/jexactyl/extensions-ro/jexactyl.web-ide-1.3.3/package.json
COPY --from=extension-builder /src/dist /opt/jexactyl/extensions-ro/jexactyl.web-ide-1.3.3/dist
COPY --from=addon-tools-extension-builder /src/package.json /opt/jexactyl/extensions-ro/jexactyl-mod-manager-tools-1.0.5/package.json
COPY --from=addon-tools-extension-builder /src/dist /opt/jexactyl/extensions-ro/jexactyl-mod-manager-tools-1.0.5/dist
COPY --from=addon-tools-extension-builder /src/prompts /opt/jexactyl/extensions-ro/jexactyl-mod-manager-tools-1.0.5/prompts
COPY disable-unsafe-tools.cjs /opt/jexactyl/bin/disable-unsafe-tools.cjs
COPY reconcile-extensions.cjs /opt/jexactyl/bin/reconcile-extensions.cjs
RUN /opt/code-server/lib/node /opt/jexactyl/bin/disable-unsafe-tools.cjs \
    && /opt/code-server/lib/node --check /opt/jexactyl/bin/reconcile-extensions.cjs \
    && /opt/code-server/lib/node --check /opt/code-server/lib/vscode/out/vs/code/browser/workbench/workbench.js \
    && /opt/code-server/lib/node --check /opt/code-server/lib/vscode/out/vs/workbench/workbench.web.main.internal.js
# Keep the filtered Copilot package at code-server's original built-in path.
# product.json declares GitHub.copilot-chat as built-in; moving it into the
# user extension directory makes the GitHub sign-in flow try to install it
# from the marketplace and fail with "extension ... was not found".
COPY --chmod=0555 entrypoint.sh /opt/jexactyl/bin/webide-entrypoint
COPY --chmod=0555 terminal-guard.sh /opt/jexactyl/bin/jexactyl-terminal
RUN /opt/code-server/lib/node --check /opt/jexactyl/bin/jexactyl-terminal
# Keep the native Execute protocol regression-tested at image build time. An
# idle prompt may advertise rich shell integration, but it must not advertise
# a command start; doing so leaves VS Code waiting on a phantom command and it
# eventually interrupts the real `.wings` input. The B marker belongs only in
# beginCommand(), immediately before the bytes are forwarded to Wings.
RUN /opt/code-server/lib/node -e 'const fs=require("fs");const s=fs.readFileSync("/opt/jexactyl/bin/jexactyl-terminal","utf8");const r=s.match(/function readyForCommand\(\) \{([\s\S]*?)\n\}/)?.[1]??"";const b=s.match(/function beginCommand\(commandLine = .+?\) \{([\s\S]*?)\n\}/)?.[1]??"";const m="shellIntegration("+String.fromCharCode(39)+"B"+String.fromCharCode(39)+")";if(r.includes(m)||!b.includes(m))throw new Error("invalid OSC 633 command marker placement");'
ENTRYPOINT ["/usr/bin/tini", "--", "/opt/jexactyl/bin/webide-entrypoint"]
CMD ["/opt/code-server/bin/code-server", "--help"]
