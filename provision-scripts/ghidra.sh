#!/bin/bash
# Installs Ghidra with JDK 21 into /opt, for headless analysis (there is no
# display for its GUI). Ghidra's release has no native decompiler for Linux
# on arm64, so it is built from the source the release ships, as is the GNU
# demangler for C++ and Rust symbol names. Adds GHIDRA_INSTALL_DIR and
# JAVA_HOME to ~/.profile.

version=12.1.4
file=ghidra_${version}_PUBLIC_20260921.zip
sha256=ddac49f903da9d5bac833e5cc79395098b9c33cfd3279be5f31bd00387d2d4db
ghidra=/opt/ghidra_${version}_PUBLIC

sudo apt-get -y install --no-install-recommends openjdk-21-jdk-headless bison

tmp=$(mktemp -d)
curl -fsSL "https://github.com/NationalSecurityAgency/ghidra/releases/download/Ghidra_${version}_build/$file" \
    -o "$tmp/$file"
echo "$sha256  $tmp/$file" | sha256sum -c -
unzip -q "$tmp/$file" -d "$tmp"

# The decompiler's Makefile only knows x86 on Linux, so name the target
# directory and leave out its -m32/-m64.
cp -r "$tmp/ghidra_${version}_PUBLIC/Ghidra/Features/Decompiler/src/decompile/cpp" "$tmp/decompiler"
make -C "$tmp/decompiler" -j"$(nproc)" ghidra_opt ARCH_TYPE= OSDIR=linux_arm_64
install -D -m 755 "$tmp/decompiler/ghidra_opt" \
    "$tmp/ghidra_${version}_PUBLIC/Ghidra/Features/Decompiler/os/linux_arm_64/decompile"

# The flags Ghidra's build.gradle passes to gcc.
demangler=$tmp/ghidra_${version}_PUBLIC/GPL/DemanglerGnu
mkdir -p "$demangler/os/linux_arm_64"
gcc -O2 -std=gnu17 -DHAVE_STDLIB_H -DHAVE_STRING_H \
    -I "$demangler/src/demangler_gnu_v2_41/headers" \
    "$demangler"/src/demangler_gnu_v2_41/c/*.c \
    -o "$demangler/os/linux_arm_64/demangler_gnu_v2_41"

sudo rm -rf "$ghidra"
sudo mv "$tmp/ghidra_${version}_PUBLIC" "$ghidra"
sudo chown -R root:root "$ghidra"
rm -rf "$tmp"

cat >> ~/.profile <<EOF
export GHIDRA_INSTALL_DIR=$ghidra
export JAVA_HOME=/usr/lib/jvm/java-21-openjdk-arm64
EOF
