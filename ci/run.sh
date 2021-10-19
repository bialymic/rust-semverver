#!/usr/bin/env sh

set -ex

OS=${1}

export RUST_BACKTRACE=full
#export RUST_TEST_NOCAPTURE=1

if [ "${OS}" = "windows" ]; then
    rustup set default-host x86_64-pc-windows-msvc
fi

cargo build
cargo test --verbose -- --nocapture

case "${OS}" in
    *"linux"*)
        TEST_TARGET=x86_64-unknown-linux-gnu cargo test --verbose -- --nocapture
        ;;
    *"windows"*)
        TEST_TARGET=x86_64-pc-windows-msvc cargo test --verbose -- --nocapture
        ;;
    *"macos"*)
        TEST_TARGET=x86_64-apple-darwin cargo test --verbose -- --nocapture
        ;;
esac

# FIXME: semververver step fails with:
# error: breaking changes in `old::changes::_::<impl serde::ser::Serialize for old::changes::ChangeCategory>`
#   --> /home/runner/.cargo/registry/src/github.com-1ecc6299db9ec823/semverver-0.1.48/src/changes.rs:35:62
#    |
# 35 | #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#    |                                                              ^^^^^^^^^
#    |
#    = warning: trait impl specialized or removed (breaking)
#    = note: this error originates in the derive macro `Serialize` (in Nightly builds, run with -Z macro-backtrace for more info)
#
# I guess this is related to serde version difference between 0.1.46 and 0.1.48
# but anyway we cannot address it here (I think), so disable it until the next release.

# install
# mkdir -p ~/rust/cargo/bin
# cp target/debug/cargo-semver ~/rust/cargo/bin
# cp target/debug/rust-semverver ~/rust/cargo/bin

# become semververver
#
# Note: Because we rely on rust nightly building the previously published
#       semver can often fail. To avoid failing the build we first check
#       if we can compile the previously published version.
# if cargo install --root "$(mktemp -d)" semverver > /dev/null 2>/dev/null; then
#     PATH=~/rust/cargo/bin:$PATH cargo semver | tee semver_out
#     current_version="$(grep -e '^version = .*$' Cargo.toml | cut -d ' ' -f 3)"
#     current_version="${current_version%\"}"
#     current_version="${current_version#\"}"
#     result="$(head -n 1 semver_out)"
#     if echo "$result" | grep -- "-> $current_version"; then
#         echo "version ok"
#         exit 0
#     else
#         echo "versioning mismatch"
#         cat semver_out
#         echo "versioning mismatch"
#         exit 1
#     fi
# else
#     echo 'Failed to check semver-compliance of semverver. Failed to compiled previous version.' >&2
# fi
