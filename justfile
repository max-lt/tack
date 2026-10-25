default:
    @just --list

# Installe le binaire `ccrs` dans ~/.cargo/bin
install:
    cargo install --path crates/cli
