#!/bin/sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$project_root/Cargo.toml" | head -n 1)
topdir="$project_root/packaging/rpmbuild"
source_dir="$topdir/SOURCES/duck-packages-$version"

mkdir -p "$topdir/BUILD" "$topdir/BUILDROOT" "$topdir/RPMS" "$topdir/SOURCES" "$topdir/SPECS" "$topdir/SRPMS"
rm -rf "$source_dir"
mkdir -p "$source_dir"

tar \
  --exclude='./.git' \
  --exclude='./.build-deps' \
  --exclude='./build' \
  --exclude='./target' \
  --exclude='./packaging/rpmbuild' \
  -C "$project_root" -cf - . | tar -C "$source_dir" -xf -

(cd "$source_dir" && cargo vendor vendor > .cargo-config.toml)
mkdir -p "$source_dir/.cargo"
mv "$source_dir/.cargo-config.toml" "$source_dir/.cargo/config.toml"
tar -C "$topdir/SOURCES" -cJf "$topdir/SOURCES/duck-packages-$version.tar.xz" "duck-packages-$version"
cp "$project_root/packaging/duck-packages.spec" "$topdir/SPECS/"

rpmbuild -bb \
  --define "_topdir $topdir" \
  "$topdir/SPECS/duck-packages.spec"

