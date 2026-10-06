build:
    cargo build --release --locked

test:
    cargo test --locked

check-comments:
    cargo test --locked --test no_comments

check: check-comments
    cargo fmt --all -- --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked

link: build
    sh scripts/link.sh

bump kind="patch":
    python3 scripts/bump-version.py {{quote(kind)}}
