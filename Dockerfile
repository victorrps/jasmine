# Stage 1: Build
FROM rust:1.82-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
# Create dummy main.rs to cache dependency build
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release 2>/dev/null || true
# Copy real source and build
COPY . .
RUN cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    tesseract-ocr \
    libtesseract-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/docforge .
COPY migrations/ ./migrations/
COPY static/ ./static/
# Create data directory for SQLite
RUN mkdir -p /app/data
ENV DATABASE_URL=sqlite:///app/data/docforge.db?mode=rwc
ENV HOST=0.0.0.0
ENV PORT=8080
EXPOSE 8080
CMD ["./docforge"]
