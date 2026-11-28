#!/bin/sh
# Regenerates googlechat.rs (prost types) and googlechat.desc (the descriptor set the pblite reader walks) from
# googlechat.proto. Needs protoc and network access for prost-build; the outputs are committed, so a normal build
# needs neither.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/src"
cat > "$work/Cargo.toml" <<'TOML'
[package]
name = "gcgen"
version = "0.1.0"
edition = "2021"
[dependencies]
prost-build = "=0.14.4"
TOML
cat > "$work/src/main.rs" <<'RS'
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = std::path::Path::new(&args[1]).parent().unwrap();
    let mut c = prost_build::Config::new();
    c.out_dir(&args[2]);
    c.compile_protos(&[args[1].as_str()], &[dir]).unwrap();
}
RS
(cd "$work" && cargo run -q --release -- "$here/googlechat.proto" "$here")
protoc -I "$here" --descriptor_set_out="$here/googlechat.desc" "$here/googlechat.proto"
