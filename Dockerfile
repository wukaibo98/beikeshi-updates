# 多阶段构建：builder 用 alpine + musl，产出的是**完全静态**的二进制，
# 所以最终镜像可以放在 scratch 里 —— 没有 shell、没有 libc、没有包管理器。
#
# 结果：镜像里只有一个 1.8MB 的可执行文件，整镜像约 4MB。
# 对比原来的 Python 版（基础镜像 45MB + 依赖 15MB），这是这次重写最实在的收益。

# 基础镜像走参数，默认还是 Docker Hub（GitHub Actions 上就是这样）。
# 国内直连 Docker Hub 会被拦，本地构建时换一个镜像源即可，不用改文件：
#   docker build --build-arg RUST_IMAGE=docker.m.daocloud.io/library/rust:1-alpine .
ARG RUST_IMAGE=rust:1-alpine
FROM ${RUST_IMAGE} AS build

# 可选的 crates 镜像：国内构建时 crates.io 会卡住（实测 16 分钟零编译），
# 传这个参数换源即可；GitHub Actions 上留空，照旧走 crates.io。
#   docker build --build-arg CARGO_MIRROR=sparse+https://rsproxy.cn/index/ ...
ARG CARGO_MIRROR=
RUN if [ -n "$CARGO_MIRROR" ]; then \
      printf '[source.crates-io]\nreplace-with = "mirror"\n\n[source.mirror]\nregistry = "%s"\n' \
        "$CARGO_MIRROR" > /usr/local/cargo/config.toml; \
    fi

# rusqlite 用 bundled 特性自带 SQLite 源码，需要 C 编译器
RUN apk add --no-cache musl-dev

WORKDIR /src

# 先只拷清单，让依赖层能被缓存：改代码不用重新编译依赖
COPY Cargo.toml Cargo.lock ./
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    mkdir src && echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src

COPY src ./src
# 需要 touch 一下，否则 cargo 认为源码没变，会复用上面那个空壳。
# target 是 cache mount（构建间复用、但不进镜像层），所以要把产物复制到普通目录。
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    touch src/main.rs && cargo build --release && \
    cp target/release/bks-updates /usr/local/bin/bks-updates

FROM scratch

COPY --from=build /usr/local/bin/bks-updates /bks-updates

ENV DATA_DIR=/data \
    BIND=0.0.0.0:8080

VOLUME ["/data"]
EXPOSE 8080

ENTRYPOINT ["/bks-updates"]
