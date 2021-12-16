# Drift Liquidator

A fast liquidator for drift written in rust. Build the liquidator by running `cargo build --release` and then run it by first placing a keypair file named `id.json` in this directory and then running `./target/release/drift-liquidator`. The keypair must have a drift account and a drift alpha ticket + enough solana for gas.

You can change the rpc node by modifying `src/config.rs`
