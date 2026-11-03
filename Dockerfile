# Builds Redlib from source. The previous Dockerfile downloaded a release
# tarball from redlib-org, which is the upstream binary, not this fork.
#
# The compiler runs on the build machine (BUILDPLATFORM) and cross-compiles
# with rust-musl-cross. BoringSSL under QEMU on arm64 is slow and often runs
# out of memory; a native cross compiler is the reliable path for Oracle ARM.
# QEMU stays available for the runtime stage, which is only Alpine plus the binary.

FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/rust-musl-cross:x86_64-musl AS cross-amd64
FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/rust-musl-cross:aarch64-musl AS cross-arm64
FROM cross-${TARGETARCH} AS builder

ARG TARGETARCH
# Architecture of the machine running the build, not the image being produced.
ARG BUILDARCH
ENV CARGO_TERM_PROGRESS_WHEN=always
ENV CARGO_TERM_PROGRESS_WIDTH=80

# rust-musl-cross already installs aarch64-unknown-linux-musl-gcc (and g++)
# and points Cargo's linker at it. gcc-aarch64-linux-gnu is a jammy cross
# package that exists on amd64 and does not exist on an arm64 host, so a
# native Oracle ARM build must not install it. It is only added when an
# amd64 builder is cross-compiling to arm64, matching scripts/build-musl-docker.sh.
RUN apt-get update \
	&& apt-get -y install git cmake perl pkg-config libclang-dev \
	&& if [ "$BUILDARCH" = "amd64" ] && [ "$TARGETARCH" = "arm64" ]; then apt-get -y install gcc-aarch64-linux-gnu g++-aarch64-linux-gnu; fi \
	&& rm -rf /var/lib/apt/lists/* \
	&& case "$TARGETARCH" in \
		amd64) echo x86_64-unknown-linux-musl > /tmp/rust-target ;; \
		arm64) echo aarch64-unknown-linux-musl > /tmp/rust-target ;; \
		*) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
	esac \
	&& rustup target add "$(cat /tmp/rust-target)"

WORKDIR /redlib

COPY Cargo.lock Cargo.toml ./
RUN mkdir src && echo 'fn main() { panic!("placeholder"); }' > src/main.rs \
	&& cargo build --release --locked --bin redlib --target "$(cat /tmp/rust-target)" \
	&& rm src/main.rs && rmdir src

COPY . ./
RUN cargo build --release --locked --bin redlib --target "$(cat /tmp/rust-target)" \
	&& cp "target/$(cat /tmp/rust-target)/release/redlib" /tmp/redlib

# Create the user on the build machine. The runtime stage then has no RUN, so
# an arm64 image does not need QEMU just to execute adduser.
FROM --platform=$BUILDPLATFORM alpine:3.23 AS accounts
RUN adduser --home /nonexistent --no-create-home --disabled-password redlib

FROM alpine:3.23

COPY --from=accounts /etc/passwd /etc/passwd
COPY --from=accounts /etc/group /etc/group
COPY --from=accounts /etc/shadow /etc/shadow
COPY --from=builder /tmp/redlib /usr/local/bin/redlib
USER redlib

EXPOSE 8080

HEALTHCHECK --interval=1m --timeout=3s CMD wget --spider -q http://localhost:8080/settings || exit 1

CMD ["redlib"]
