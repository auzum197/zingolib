#!/bin/bash
# Builds, signs and zips watchonly-tui for every release target.
#
# The Linux and Windows builds run in a docker image. To build it:
# cd docker
# docker build --tag rustbuild:latest .
#
# Set CODESIGN_IDENTITY to sign the macOS binary.

set -e

while [[ $# -gt 0 ]]
do
case $1 in
    -v|--version)
    APP_VERSION="$2"
    shift
    shift
    ;;
    *)
    shift
    ;;
esac
done

if [ -z "$APP_VERSION" ]; then echo "APP_VERSION is not set"; exit 1; fi

BIN=watchonly-tui

cargo test --release -p $BIN

# package <platform> <path of the built binary>
package () {
    local dir=$1-$BIN-v$APP_VERSION
    local file
    file=$(basename "$2")
    rm -rf "target/$dir"
    mkdir -p "target/$dir"
    cp "$2" "target/$dir/"
    gpg --batch --output "target/$dir/$BIN.sig" --detach-sig "target/$dir/$file"
    (cd "target/$dir" && gsha256sum "$file" > sha256sum.txt)
    (cd target && zip -r "$dir.zip" "$dir")
}

# macOS builds on the host
cargo build --release -p $BIN
if [ -n "$CODESIGN_IDENTITY" ]; then
    codesign -f -s "$CODESIGN_IDENTITY" target/release/$BIN --deep
fi
package macOS target/release/$BIN

docker run --rm -v "$(pwd)/:/opt/ekdromos" rustbuild:latest bash -c "cd /opt/ekdromos && cargo build --release -p $BIN && cargo build --release -p $BIN --target armv7-unknown-linux-gnueabihf && cargo build --release -p $BIN --target aarch64-unknown-linux-gnu && cargo build --release -p $BIN --target x86_64-pc-windows-gnu"

package linux target/release/$BIN
package Windows target/x86_64-pc-windows-gnu/release/$BIN.exe
package Armv7 target/armv7-unknown-linux-gnueabihf/release/$BIN
package aarch64 target/aarch64-unknown-linux-gnu/release/$BIN
