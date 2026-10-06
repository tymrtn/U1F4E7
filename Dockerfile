# Envelope's MCP server in a container. MCP directories such as Glama build
# this image to start the server and list its tools; see
# docker/mcp-entrypoint.sh for how it starts without a token.
#
#   docker build -t envelope .
#   docker run -i --rm envelope
#
# Release builds are made without the Governor send gate, and so is this one.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked --bin envelope

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 envelope \
    && install -d -m 700 -o envelope -g envelope /home/envelope/data
COPY --from=build /src/target/release/envelope /usr/local/bin/envelope
COPY docker/mcp-entrypoint.sh /usr/local/bin/envelope-mcp-entrypoint
USER envelope
ENV ENVELOPE_HOME=/home/envelope/data
ENTRYPOINT ["envelope-mcp-entrypoint"]
