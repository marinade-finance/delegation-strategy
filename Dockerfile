FROM rust:latest AS builder

# RUN rustup target add x86_64-unknown-linux-musl
RUN apt update && apt install -y musl-tools musl-dev pkg-config libssl-dev libudev-dev bash sqlite3 curl
RUN update-ca-certificates

WORKDIR /usr/local/

RUN sh -c "$(curl -sSfL https://release.solana.com/stable/install)"
ENV PATH="/root/.local/share/solana/install/active_release/bin:$PATH"
RUN chmod 755 -R /root
RUN solana config set -u mainnet-beta
RUN cp -r /root/.config /
RUN chmod 755 /.config

ENV HOME="/root"

ADD Cargo.lock .
ADD Cargo.toml .
ADD cli cli/
RUN cargo build

ADD scripts scripts/
ADD sql sql/
