# meerkat

[![CI](https://github.com/meerkat-lang/meerkat/actions/workflows/ci.yml/badge.svg)](https://github.com/meerkat-lang/meerkat/actions/workflows/ci.yml)
[![Weekly Security](https://github.com/meerkat-lang/meerkat/actions/workflows/weekly-security.yml/badge.svg)](https://github.com/meerkat-lang/meerkat/actions/workflows/weekly-security.yml)

The implementation of the Meerkat language

## Repository Structure

This repository contains the Meerkat distributed reactive programming system, organized into the following packages:

### meerkat-lib

Core libraries for the Meerkat runtime:

- **net** - Network layer with libp2p and circuit relay support for peer-to-peer communication

### Building and Testing

We use [`pre-commit`](https://pre-commit.com/) to ensure code quality.

1. **Install pre-commit:** Run `sudo apt install pre-commit` or `pip install pre-commit` or `brew install pre-commit` (see [installation guide](https://pre-commit.com/#install)).
2. **Set up hooks:** Run `pre-commit install` in the repository root before submitting a pull request.

```bash
# Build all packages
cargo build

# Run all tests
cargo test

# Install the WASM target (one-time setup)
rustup target add wasm32-unknown-unknown

# Test WASM compatibility
cargo check --locked -p meerkat-lib --target wasm32-unknown-unknown --all-features

# Run the REPL
cargo run

# Run a simple example network-accessible service; prints a URL to connect to
cargo run -- -s -f meerkat/tests/s1.mkt

# Connect to a remote service and run tests
cargo run -- -f meerkat/tests/test_client.mkt -i "<Service URL>"

# Run an integration test with a distributed server and client
python3 scripts/mkn.py scripts/manifest_dist_commit.json

# Watch def updates in real time
cargo run -- -s -f meerkat/tests/s1.mkt                                  # (in terminal 1): sets up a server, prints a Service URL
cargo run -- -f meerkat/tests/test_client.mkt --watch -i <Service URL>   # (in terminal 2): watches updates to local defs
cargo run -- -f meerkat/tests/test_client.mkt -i <Service URL>           # (in terminal 3): modifies a var on s1, triggers an update to the test_client in terminal 2

```

## License

MIT
