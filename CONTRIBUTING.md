# Contributing

1. Install stable Rust and clone the repository.
2. Create a focused branch and keep provider secrets in environment variables.
3. Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets`.
4. Add tests for protocol, persistence, detection, permission, or rendering behavior that changes.
5. Integration tests live in `tests/` and must stay hermetic: bind `127.0.0.1` on an ephemeral port, point `HOME` at a temporary directory, and never call a real provider or CTFd instance.
6. Do not commit `.ctf/flags.log`, `.ctf/errors.log`, databases, API keys, or challenge secrets.
