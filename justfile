# deepseek-harness-rs verify 全集
# 用法:just verify

default:
    @just --list

# verify 脚本全集:格式 / lint / 测试 / 契约 / e2e / 链接
verify: fmt-check clippy test wit component-contracts e2e links
    @echo "verify: ALL GREEN"

# 桌面客户端(GPUI 原生 UI):构建 dsh-desktop;运行用 just desktop-run
desktop:
    cargo build -p dsh-desktop

# 桌面客户端运行;参数透传,如 just desktop-run --fake
desktop-run ARGS='':
    cargo run -p dsh-desktop -- {{ARGS}}

wit:
    bash scripts/verify-wit

component-contracts:
    bash scripts/verify-component-contracts

e2e:
    bash scripts/verify-e2e

links:
    python3 scripts/verify-links

fmt-check:
    cargo fmt --all --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace
