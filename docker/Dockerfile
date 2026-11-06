# Builds the NAS service. Run from the repo root:
#   docker build -f docker/Dockerfile -t card-intake-server .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p card_intake_server

FROM debian:bookworm-slim
COPY --from=build /src/target/release/card_intake_server /usr/local/bin/card_intake_server
ENV CARD_INTAKE_PHOTOS=/photos \
    PORT=8787
EXPOSE 8787
CMD ["card_intake_server"]
