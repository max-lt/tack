default:
    @just --list

# Installe le binaire `tack` dans ~/.cargo/bin
install:
    cargo install --path crates/cli
