FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
# Explicit musl target keeps the binary fully static (rustls has embedded CA roots,
# so FROM scratch needs no /etc/ssl).
RUN cargo build --release --target "$(uname -m)-unknown-linux-musl" \
 && cp "target/$(uname -m)-unknown-linux-musl/release/varde" /varde

FROM scratch
COPY --from=build /varde /varde
EXPOSE 3000
ENTRYPOINT ["/varde"]
