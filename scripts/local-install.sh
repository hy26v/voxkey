#!/usr/bin/env bash
# ABOUTME: Builds a uniquely versioned local Voxkey RPM from the current checkout.
# ABOUTME: Installs through DNF so every local system file remains RPM-owned.

set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

build_only=false
case "${1:-}" in
    "") ;;
    --build-only) build_only=true ;;
    *)
        printf 'Usage: %s [--build-only]\n' "$0" >&2
        exit 2
        ;;
esac

required_commands=(cargo rpmbuild magick curl sha256sum git rpm)
if [[ "$build_only" == false ]]; then
    required_commands+=(dnf sudo systemctl)
fi
for command_name in "${required_commands[@]}"; do
    if ! command -v "$command_name" >/dev/null; then
        printf 'Required command not found: %s\n' "$command_name" >&2
        exit 1
    fi
done

build_stamp="$(date -u +%Y%m%d%H%M%S%N)"
git_revision="$(git rev-parse --short=8 HEAD)"
dirty_suffix=""
if [[ -n "$(git status --porcelain --untracked-files=normal)" ]]; then
    dirty_suffix=".dirty"
fi
rpm_dist="local.${build_stamp}.git${git_revision}${dirty_suffix}"
rpm_topdir="$project_root/rpmbuild/$rpm_dist"
rpm_sources="$rpm_topdir/SOURCES"
mkdir -p "$rpm_sources/gnome-shell-extension"

printf '%s\n' 'Building release binaries for the RPM...' >&2
existing_rustflags="${RUSTFLAGS:-}"
package_rustflags="${existing_rustflags:+$existing_rustflags }-C link-arg=-Wl,-rpath,/usr/lib64/voxkey"
RUSTFLAGS="$package_rustflags" \
    cargo build --release --locked -p voxkey -p voxkey-settings

printf '%s\n' 'Preparing RPM sources...' >&2
cp target/release/voxkey target/release/voxkey-settings "$rpm_sources/"
cp scripts/keyboard-recovery "$rpm_sources/"
cp target/release/deps/libonnxruntime.so "$rpm_sources/"
cp target/release/deps/libsherpa-onnx-c-api.so "$rpm_sources/"
cp data/voxkey.service data/io.github.hy26v.Voxkey.desktop \
    data/io.github.hy26v.Voxkey.metainfo.xml data/90-voxkey.preset \
    "$rpm_sources/"
cp logo.png "$rpm_sources/io.github.hy26v.Voxkey-512.png"
magick logo.png -resize 256x256 "$rpm_sources/io.github.hy26v.Voxkey-256.png"
magick logo.png -resize 128x128 "$rpm_sources/io.github.hy26v.Voxkey-128.png"
cp gnome-shell-extension/metadata.json gnome-shell-extension/extension.js \
    gnome-shell-extension/constants.js gnome-shell-extension/util.js \
    gnome-shell-extension/capsule.js gnome-shell-extension/toggle.js \
    gnome-shell-extension/stylesheet.css \
    "$rpm_sources/gnome-shell-extension/"

vad_filename="ggml-silero-v6.2.0.bin"
vad_sha256="2aa269b785eeb53a82983a20501ddf7c1d9c48e33ab63a41391ac6c9f7fb6987"
vad_cache_dir="$project_root/rpmbuild/cache"
vad_cache_path="$vad_cache_dir/$vad_filename"
mkdir -p "$vad_cache_dir"
if ! printf '%s  %s\n' "$vad_sha256" "$vad_cache_path" \
    | sha256sum --check --strict >/dev/null 2>&1; then
    printf '%s\n' 'Downloading the pinned whisper.cpp VAD model...' >&2
    vad_download_path="$(mktemp "$vad_cache_dir/.${vad_filename}.XXXXXX")"
    cleanup_download() {
        if [[ -n "${vad_download_path:-}" && -f "$vad_download_path" ]]; then
            unlink "$vad_download_path"
        fi
    }
    trap cleanup_download EXIT
    curl --fail --location --retry 3 \
        --output "$vad_download_path" \
        "https://huggingface.co/ggml-org/whisper-vad/resolve/main/$vad_filename"
    printf '%s  %s\n' "$vad_sha256" "$vad_download_path" \
        | sha256sum --check --strict
    mv "$vad_download_path" "$vad_cache_path"
    vad_download_path=""
fi
cp "$vad_cache_path" "$rpm_sources/$vad_filename"

printf 'Building RPM with release suffix %s...\n' "$rpm_dist" >&2
rpmbuild --define "_topdir $rpm_topdir" \
    --define "_sourcedir $rpm_sources" \
    --define "dist .$rpm_dist" \
    -bb "$project_root/voxkey.spec" >&2

rpm_path="$(find "$rpm_topdir/RPMS" -type f -name 'voxkey-*.rpm' -print -quit)"
if [[ -z "$rpm_path" ]]; then
    printf '%s\n' 'RPM build completed without producing a Voxkey package.' >&2
    exit 1
fi
rpm -K "$rpm_path" >&2
printf 'Built %s\n' "$rpm_path" >&2

if [[ "$build_only" == true ]]; then
    printf '%s\n' "$rpm_path"
    exit 0
fi

service_was_active=false
if systemctl --user is-active --quiet voxkey.service; then
    service_was_active=true
fi

printf 'Installing %s through DNF...\n' "$rpm_path" >&2
sudo dnf install -y "$rpm_path"
systemctl --user daemon-reload
if [[ "$service_was_active" == true ]]; then
    systemctl --user restart voxkey.service
fi

printf '%s\n' 'Installed through RPM. Fully close and reopen Voxkey settings to load the new UI.' >&2
printf '%s\n' "$rpm_path"
