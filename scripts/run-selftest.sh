#!/bin/bash
# run-selftest.sh — SABOS の自動テストを実行するスクリプト
#
# QEMU を起動し、selftest コマンドを送信して結果を検証する。
# CI での利用を想定しており、全テスト PASS なら終了コード 0、
# 1 つでも FAIL なら終了コード 1 を返す。

set -e

# 色付き出力用
RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m' # No Color

# プロジェクトルートへ移動
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

# 一時ファイル
LOG_FILE="/tmp/sabos-selftest-$$.log"
MONITOR_PORT=55582

# クリーンアップ関数
cleanup() {
    if [ -n "$QEMU_PID" ] && kill -0 "$QEMU_PID" 2>/dev/null; then
        kill "$QEMU_PID" 2>/dev/null || true
        wait "$QEMU_PID" 2>/dev/null || true
    fi
    rm -f "$LOG_FILE"
}
trap cleanup EXIT

# 既存の QEMU プロセスを終了
pkill -9 -f "qemu-system-x86_64.*sabos" 2>/dev/null || true
sleep 1

# EFI をコピー
cp kernel/target/x86_64-unknown-uefi/debug/sabos.efi esp/EFI/BOOT/BOOTX64.EFI

echo "Starting QEMU..."

# QEMU を起動
qemu-system-x86_64 \
    -nodefaults \
    -machine q35 \
    -vga std \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_VARS_4M.fd \
    -drive format=raw,file=fat:rw:esp \
    -drive if=virtio,format=raw,file=disk.img \
    -netdev user,id=net0 -device virtio-net-pci,netdev=net0 \
    -serial stdio \
    -display none \
    -monitor telnet:127.0.0.1:$MONITOR_PORT,server,nowait > "$LOG_FILE" 2>&1 &

QEMU_PID=$!
echo "QEMU PID: $QEMU_PID"

# ユーザーシェルプロンプトが表示されるまで待機
echo "Waiting for user shell prompt..."
for i in {1..30}; do
    if grep -q "user>" "$LOG_FILE" 2>/dev/null; then
        break
    fi
    sleep 1
done

if ! grep -q "user>" "$LOG_FILE" 2>/dev/null; then
    echo -e "${RED}ERROR: User shell prompt not found after 30 seconds${NC}"
    cat "$LOG_FILE"
    exit 1
fi

echo "Sending user shell mkdir command..."

# mkdir TESTDIR
for c in m k d i r spc t e s t d i r ret; do
    echo "sendkey $c" | nc -q 1 127.0.0.1 $MONITOR_PORT > /dev/null 2>&1 || true
    sleep 0.25
done

echo "Waiting for mkdir output..."
for i in {1..10}; do
    if grep -q "Directory created successfully" "$LOG_FILE" 2>/dev/null; then
        break
    fi
    sleep 1
done

if ! grep -q "Directory created successfully" "$LOG_FILE" 2>/dev/null; then
    echo -e "${RED}ERROR: mkdir output not found${NC}"
    cat "$LOG_FILE"
    exit 1
fi

echo "Sending user shell rmdir command..."

# rmdir TESTDIR
for c in r m d i r spc t e s t d i r ret; do
    echo "sendkey $c" | nc -q 1 127.0.0.1 $MONITOR_PORT > /dev/null 2>&1 || true
    sleep 0.25
done

echo "Waiting for rmdir output..."
for i in {1..10}; do
    if grep -q "Directory removed successfully" "$LOG_FILE" 2>/dev/null; then
        break
    fi
    sleep 1
done

if ! grep -q "Directory removed successfully" "$LOG_FILE" 2>/dev/null; then
    echo -e "${RED}ERROR: rmdir output not found${NC}"
    cat "$LOG_FILE"
    exit 1
fi

echo "Sending user shell ls command..."

# user シェルで ls を実行
for c in l s ret; do
    echo "sendkey $c" | nc -q 1 127.0.0.1 $MONITOR_PORT > /dev/null 2>&1 || true
    sleep 0.25
done

# ls の結果を待つ（最大 10 秒）
echo "Waiting for ls output..."
for i in {1..10}; do
    if grep -q "HELLO.TXT" "$LOG_FILE" 2>/dev/null; then
        break
    fi
    sleep 1
done

if ! grep -q "HELLO.TXT" "$LOG_FILE" 2>/dev/null; then
    echo -e "${RED}ERROR: ls output did not contain HELLO.TXT${NC}"
    cat "$LOG_FILE"
    exit 1
fi

echo "Sending selftest command..."

# user シェルで selftest を実行
for c in s e l f t e s t ret; do
    echo "sendkey $c" | nc -q 1 127.0.0.1 $MONITOR_PORT > /dev/null 2>&1 || true
    sleep 0.25
done

# テスト結果を待つ（最大 30 秒）
echo "Waiting for selftest to complete..."
for i in {1..30}; do
    if grep -q "SELFTEST END" "$LOG_FILE" 2>/dev/null; then
        break
    fi
    sleep 1
done

# 結果確認前に少し待つ
sleep 2

# QEMU を終了
kill "$QEMU_PID" 2>/dev/null || true
wait "$QEMU_PID" 2>/dev/null || true
QEMU_PID=""  # cleanup で再度 kill しないように

# 結果を表示
echo ""
echo "========== SELFTEST OUTPUT =========="
grep -E "(SELFTEST|PASS|FAIL)" "$LOG_FILE" || true
echo "====================================="
echo ""

# 結果を検証
if grep -q "SELFTEST END:.*PASSED" "$LOG_FILE"; then
    echo -e "${GREEN}All tests PASSED!${NC}"
    exit 0
else
    echo -e "${RED}Some tests FAILED!${NC}"
    echo ""
    echo "Full log:"
    cat "$LOG_FILE"
    exit 1
fi
