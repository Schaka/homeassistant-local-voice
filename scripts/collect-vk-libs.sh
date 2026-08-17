#!/usr/bin/env bash
# Collect the host's RADV Vulkan driver + its distro-specific dependencies into
# ./vk-libs/, then emit ./radeon_icd.container.json pointing at them.
#
# Why this exists: the image runs ggml's Vulkan backend on a small AMD GPU
# (Oland/GCN1). The host mesa driver works but is linked against the host
# distro's sonames (libLLVM.so.22, libedit.so.0, libxml2.so.2, ...) which do
# not exist in the container's base distro. We copy the driver and only those
# mismatched libs into the image; everything else resolves from the container's
# own packages.
#
# Run on the GPU host, once, before `./scripts/build.sh`:
#   ./scripts/collect-vk-libs.sh
set -eu
cd "$(dirname "$0")/.."

OUT=./vk-libs
mkdir -p "$OUT"

copy_soname() {
    # $1 = soname, e.g. libedit.so.0
    local name="$1"
    local src
    src=$(find /lib64 /usr/lib64 -name "$name" 2>/dev/null | head -1)
    if [ -z "$src" ]; then
        echo "WARNING: $name not found on host (searching /lib64 /usr/lib64)" >&2
        return
    fi
    # Copy the real file (follow symlinks) under both the soname and its real name.
    local real
    real=$(readlink -f "$src")
    cp -L "$src" "$OUT/$name"
    cp -L "$real" "$OUT/$(basename "$real")"
    echo "collected $name -> $(basename "$real")"
}

copy_soname libvulkan.so.1
copy_soname libvulkan_radeon.so
copy_soname libLLVM.so.22.1
copy_soname libSPIRV-Tools.so
copy_soname libdisplay-info.so.3
copy_soname libedit.so.0
copy_soname libxml2.so.2

# Make sure the loader (libvulkan.so.1) can find the driver by soname.
[ -f "$OUT/libvulkan.so.1" ] || { echo "ERROR: libvulkan.so.1 not collected" >&2; exit 1; }

cat > ./radeon_icd.container.json <<'EOF'
{
    "ICD": {
        "api_version": "1.4.354",
        "library_arch": "64",
        "library_path": "/opt/vk/libvulkan_radeon.so"
    },
    "file_format_version": "1.0.1"
}
EOF

echo
echo "Collected driver into $OUT:"
ls -la "$OUT"
echo
echo "ICD manifest written to ./radeon_icd.container.json"