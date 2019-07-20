FROM alpine:3.10 AS builder
RUN apk add --no-cache rust cargo openssl-dev
WORKDIR /usr/src/otterhound
COPY Cargo.* ./
COPY src ./src
RUN cargo build --release --bin otterhound

FROM alpine:3.10
RUN apk add --no-cache libgcc openssl
COPY --from=builder /usr/src/otterhound/target/release/otterhound /usr/bin/
CMD ["otterhound"]
