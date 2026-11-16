# The wits node agent. Build from the repository root:
#   docker build -f deploy/Dockerfile -t wits .
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release --locked && strip target/release/wits

FROM alpine:3.20
COPY --from=build /src/target/release/wits /usr/local/bin/wits
ENTRYPOINT ["/usr/local/bin/wits"]
CMD ["agent"]
