# Image for running zx-agent in a resident Pod: zx-agent with bash, kubectl, curl and jq.
# Build from the repository root (the build context must be the repository):
#   docker build -f deploy/kubernetes/Dockerfile -t <registry>/zx-agent:<tag> .

# The toolchain the release workflow uses (.github/workflows/release.yml).
FROM rust:1.88-slim-bookworm AS build
WORKDIR /src
# Only what the build reads (profiles/ is compiled in), so edits elsewhere keep it cached.
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY profiles profiles
RUN cargo build --release --locked

FROM debian:bookworm-slim
# Keep kubectl within one minor version of the clusters it talks to.
ARG KUBECTL_VERSION=v1.36.1
# Set by BuildKit to the platform being built, such as amd64 or arm64.
ARG TARGETARCH
# tini runs as PID 1 in the resident Pod: it reaps orphaned processes and forwards SIGTERM.
RUN apt-get update \
 && apt-get install -y --no-install-recommends bash ca-certificates curl jq tini \
 && rm -rf /var/lib/apt/lists/*
# Checked against the checksum published next to the binary, as the Kubernetes install
# guide does.
RUN curl -fsSLo /usr/local/bin/kubectl "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/${TARGETARCH}/kubectl" \
 && echo "$(curl -fsSL "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/${TARGETARCH}/kubectl.sha256")  /usr/local/bin/kubectl" \
    | sha256sum --check \
 && chmod 0755 /usr/local/bin/kubectl
COPY --from=build /src/target/release/zx-agent /usr/local/bin/zx-agent
# A fixed non-root user; the manifests run the Pod with the same IDs. /data is its home and
# the mount point of the session volume; it exists in the image so that Pods without the
# volume, such as scheduled inspections with a read-only root filesystem, can start in it.
RUN groupadd --gid 10001 zx \
 && useradd --uid 10001 --gid 10001 --home-dir /data --no-create-home --shell /bin/bash zx \
 && mkdir /data \
 && chown 10001:10001 /data
USER 10001:10001
WORKDIR /data
