#!/usr/bin/env bash
set -Eeuo pipefail

readonly ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
readonly TARGET="aarch64-unknown-linux-gnu"
readonly TARGET_GLIBC="2.36"
readonly TARGET_ZIG_TRIPLE="aarch64-linux-gnu.${TARGET_GLIBC}"
readonly DEBIAN_ARCH="arm64"
readonly MULTIARCH_DIR="aarch64-linux-gnu"
readonly SYSROOT_IMAGE="debian:12-slim@sha256:ec31d744f83f4a73447ba045aaab9409997efe5cc706eca4890c1be36cc1606a"
readonly SYSROOT="${ROOT_DIR}/target/zigbuild-sysroot-arm64-bookworm-glibc-2.36"
readonly TOOL_DIR="${ROOT_DIR}/target/zigbuild-tools-arm64"

if [[ "$#" -ne 0 ]]; then
  printf 'Usage: bash scripts/build-linux-arm64.sh\n' >&2
  exit 2
fi

if [[ "$(uname -s)" != "Linux" || "$(uname -m)" != "x86_64" ]]; then
  printf 'This cross-build helper requires an x86_64 Linux host; it never emulates the target.\n' >&2
  exit 1
fi

for required_command in docker mise pkg-config; do
  if ! command -v "$required_command" >/dev/null 2>&1; then
    printf 'Required local command not found: %s\n' "$required_command" >&2
    exit 1
  fi
done

readonly DOCKER_CONTEXT="$(docker context show)"
readonly DOCKER_ENDPOINT="$(docker context inspect "$DOCKER_CONTEXT" --format '{{.Endpoints.docker.Host}}')"
case "$DOCKER_ENDPOINT" in
  unix://*) ;;
  *)
    printf 'Refusing a non-local Docker context while preparing the ARM64 sysroot.\n' >&2
    exit 1
    ;;
esac

readonly DOCKER_SECURITY_OPTIONS="$(docker --context "$DOCKER_CONTEXT" info --format '{{json .SecurityOptions}}')"
if [[ "$DOCKER_SECURITY_OPTIONS" != *'"name=rootless"'* ]]; then
  printf 'A verified rootless Docker daemon is required to prepare the local sysroot.\n' >&2
  exit 1
fi

readonly DOCKER_ARCH="$(docker --context "$DOCKER_CONTEXT" info --format '{{.Architecture}}')"
case "$DOCKER_ARCH" in
  x86_64|amd64) ;;
  *)
    printf 'The local Docker daemon must run natively on x86_64; refusing emulation.\n' >&2
    exit 1
    ;;
esac

readonly ZIG_BIN="$(mise exec -- sh -c 'command -v zig')"
mise exec -- cargo zigbuild --help >/dev/null

mkdir -p "$SYSROOT"
if [[ ! -f "$SYSROOT/.ready" ]]; then
  printf 'Preparing the Debian 12 ARM64 sysroot using a native x86_64 container (no Buildx/QEMU)...\n' >&2
  docker --context "$DOCKER_CONTEXT" run \
    --rm \
    --interactive \
    --platform linux/amd64 \
    --volume "$SYSROOT:/sysroot" \
    "$SYSROOT_IMAGE" \
    bash -se -- "$DEBIAN_ARCH" "$MULTIARCH_DIR" <<'SYSROOT_SCRIPT'
set -Eeuo pipefail

debian_arch="$1"
multiarch_dir="$2"
if [[ "$(dpkg --print-architecture)" != "amd64" ]]; then
  printf 'The sysroot container must run as native amd64.\n' >&2
  exit 1
fi

dpkg --add-architecture "$debian_arch"
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends \
  "libasound2-dev:${debian_arch}" \
  "libfontconfig-dev:${debian_arch}" \
  "libfreetype-dev:${debian_arch}" \
  "libvulkan-dev:${debian_arch}" \
  "libwayland-dev:${debian_arch}" \
  "libx11-xcb-dev:${debian_arch}" \
  "libxcb-render0-dev:${debian_arch}" \
  "libxcb-shape0-dev:${debian_arch}" \
  "libxcb-xfixes0-dev:${debian_arch}" \
  "libxcb1-dev:${debian_arch}" \
  "libxkbcommon-dev:${debian_arch}" \
  "libxkbcommon-x11-dev:${debian_arch}" \
  pkg-config

mkdir -p \
  "/sysroot/usr/include" \
  "/sysroot/usr/lib/${multiarch_dir}" \
  "/sysroot/usr/share/pkgconfig" \
  "/sysroot/lib/${multiarch_dir}"
cp -a /usr/include/. /sysroot/usr/include/
if [[ -d /sysroot/usr/include/x86_64-linux-gnu ]]; then
  rm -rf /sysroot/usr/include/x86_64-linux-gnu
fi
for header in asm bits gnu; do
  if [[ -e "/usr/include/${multiarch_dir}/${header}" ]]; then
    rm -rf "/sysroot/usr/include/${header}"
    ln -s "${multiarch_dir}/${header}" "/sysroot/usr/include/${header}"
  fi
done
cp -a "/usr/lib/${multiarch_dir}/." "/sysroot/usr/lib/${multiarch_dir}/"
cp -a "/lib/${multiarch_dir}/." "/sysroot/lib/${multiarch_dir}/"
if [[ -d "/usr/share/pkgconfig" ]]; then
  cp -a /usr/share/pkgconfig/. /sysroot/usr/share/pkgconfig/
fi
SYSROOT_SCRIPT
  printf '%s\n' "$SYSROOT_IMAGE" >"$SYSROOT/.ready"
fi

if [[ "$(<"$SYSROOT/.ready")" != "$SYSROOT_IMAGE" ]]; then
  printf 'The cached sysroot marker does not match the pinned image; refusing to use it.\n' >&2
  exit 1
fi

mkdir -p "$TOOL_DIR"
readonly CC_WRAPPER="${TOOL_DIR}/aarch64-linux-gnu-cc"
cat >"$CC_WRAPPER" <<'CC_SCRIPT'
#!/usr/bin/env bash
set -Eeuo pipefail
arguments=()
skip_target=0
for argument in "$@"; do
  if [[ "$skip_target" -eq 1 ]]; then
    skip_target=0
    continue
  fi
  case "$argument" in
    -target|--target)
      skip_target=1
      ;;
    -target=*|--target=*) ;;
    *)
      arguments+=("$argument")
      ;;
  esac
done
if [[ "$skip_target" -eq 1 ]]; then
  printf 'The C build passed a target flag without its value.\n' >&2
  exit 2
fi
exec "$CURURU_ZIG_BIN" cc \
  -target "$CURURU_ZIG_TARGET" \
  --sysroot "$CURURU_SYSROOT" \
  "${arguments[@]}"
CC_SCRIPT
chmod 0755 "$CC_WRAPPER"

export CURURU_ZIG_BIN="$ZIG_BIN"
export CURURU_ZIG_TARGET="$TARGET_ZIG_TRIPLE"
export CURURU_SYSROOT="$SYSROOT"
export CC_aarch64_unknown_linux_gnu="$CC_WRAPPER"
export CFLAGS_aarch64_unknown_linux_gnu="--sysroot=${SYSROOT}"
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR="$SYSROOT"
export PKG_CONFIG_LIBDIR="${SYSROOT}/usr/lib/${MULTIARCH_DIR}/pkgconfig:${SYSROOT}/usr/share/pkgconfig"
unset PKG_CONFIG_PATH

mise exec -- rustup target add "$TARGET"
mise exec -- cargo zigbuild --locked --release --workspace --target "${TARGET}.${TARGET_GLIBC}"

printf 'Built Linux ARM64 executable: %s/target/%s/release/cururu-desktop\n' "$ROOT_DIR" "$TARGET"
