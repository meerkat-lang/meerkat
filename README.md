# meerkat

The implementation of the Meerkat language

## Repository Structure

This repository contains the Meerkat distributed reactive programming system, organized into the following packages:

### meerkat-lib

Core libraries for the Meerkat runtime:

- **net** - Network layer with libp2p and circuit relay support for peer-to-peer communication

### Building and Testing
```bash
# Build all packages
cargo build

# Run all tests
cargo test

# Test WASM compatibility
cargo build -p meerkat-net --target wasm32-unknown-unknown
```

## License

MIT
