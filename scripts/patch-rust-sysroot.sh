#!/bin/bash
# patch-rust-sysroot.sh — SABOS 用の sysroot パッチを適用する
#
# Rust nightly の sysroot ソースに SABOS 向けの PAL / alloc / stdio パッチを当てる。
# -Zbuild-std で std をビルドするために必要。
#
# 使い方:
#   bash scripts/patch-rust-sysroot.sh
#
# 設計:
# - idempotent: 既にパッチ済みならスキップする
# - パッチ元ファイルは rust-std-sabos/ に保存してある
# - sysroot のソースを直接編集する（rust-src コンポーネントの再インストールでリセット可能）

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PATCH_DIR="$PROJECT_DIR/rust-std-sabos"

# sysroot パスを取得
SYSROOT="$(rustc +nightly --print sysroot)"
STD_SRC="$SYSROOT/lib/rustlib/src/rust/library/std/src"

echo "=== SABOS sysroot patch ==="
echo "Sysroot: $SYSROOT"
echo "std source: $STD_SRC"
echo "Patch files: $PATCH_DIR"
echo ""

# ---- 1. PAL ディレクトリの作成とファイルコピー ----

PAL_DIR="$STD_SRC/sys/pal/sabos"
if [ -d "$PAL_DIR" ]; then
    echo "[SKIP] $PAL_DIR already exists"
else
    echo "[CREATE] $PAL_DIR"
    mkdir -p "$PAL_DIR"
fi

# PAL ファイルをコピー（常に上書き — パッチの更新を反映するため）
echo "[COPY] sys/pal/sabos/mod.rs"
cp "$PATCH_DIR/sys_pal_sabos_mod.rs" "$PAL_DIR/mod.rs"

echo "[COPY] sys/pal/sabos/common.rs"
cp "$PATCH_DIR/sys_pal_sabos_common.rs" "$PAL_DIR/common.rs"

echo "[COPY] sys/pal/sabos/os.rs"
cp "$PATCH_DIR/sys_pal_sabos_os.rs" "$PAL_DIR/os.rs"

# ---- 2. alloc ファイルのコピー ----

echo "[COPY] sys/alloc/sabos.rs"
cp "$PATCH_DIR/sys_alloc_sabos.rs" "$STD_SRC/sys/alloc/sabos.rs"

# ---- 3. stdio ファイルのコピー ----

echo "[COPY] sys/stdio/sabos.rs"
cp "$PATCH_DIR/sys_stdio_sabos.rs" "$STD_SRC/sys/stdio/sabos.rs"

# ---- 3b. random ファイルのコピー ----

echo "[COPY] sys/random/sabos.rs"
cp "$PATCH_DIR/sys_random_sabos.rs" "$STD_SRC/sys/random/sabos.rs"

# ---- 3c. fs ファイルのコピー ----

echo "[COPY] sys/fs/sabos.rs"
cp "$PATCH_DIR/sys_fs_sabos.rs" "$STD_SRC/sys/fs/sabos.rs"

# ---- 3d. time ファイルのコピー ----

echo "[COPY] sys/time/sabos.rs"
cp "$PATCH_DIR/sys_time_sabos.rs" "$STD_SRC/sys/time/sabos.rs"

# ---- 3e. env ファイルのコピー ----

echo "[COPY] sys/env/sabos.rs"
cp "$PATCH_DIR/sys_env_sabos.rs" "$STD_SRC/sys/env/sabos.rs"

# ---- 3g. net/connection ファイルのコピー ----

echo "[COPY] sys/net/connection/sabos.rs"
cp "$PATCH_DIR/sys_net_connection_sabos.rs" "$STD_SRC/sys/net/connection/sabos.rs"

# ---- 3f. os/sabos ディレクトリの作成とファイルコピー ----

OS_SABOS_DIR="$STD_SRC/os/sabos"
if [ -d "$OS_SABOS_DIR" ]; then
    echo "[SKIP] $OS_SABOS_DIR already exists"
else
    echo "[CREATE] $OS_SABOS_DIR"
    mkdir -p "$OS_SABOS_DIR"
fi

echo "[COPY] os/sabos/mod.rs"
cp "$PATCH_DIR/os_sabos_mod.rs" "$OS_SABOS_DIR/mod.rs"

echo "[COPY] os/sabos/ffi.rs"
cp "$PATCH_DIR/os_sabos_ffi.rs" "$OS_SABOS_DIR/ffi.rs"

# ---- 4. 既存ファイルへのパッチ（Python で正確に処理） ----

python3 "$SCRIPT_DIR/apply-sysroot-patches.py" "$STD_SRC"

echo ""
echo "=== Patch complete ==="
echo "You can now build with: cargo build -Zbuild-std=core,alloc,std --target x86_64-sabos.json"
