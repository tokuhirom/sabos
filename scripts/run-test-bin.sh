#!/bin/bash
# run-test-bin.sh — 特定のユーザーバイナリを /host/ 経由でテスト実行するスクリプト
#
# QEMU を起動し、ユーザーシェルで /host/<BIN>.ELF を実行して出力をキャプチャする。
# disk.img の再作成をスキップし、hostfs.img のインクリメンタル更新のみで高速にテストを回す。
#
# 使い方:
#   ./scripts/run-test-bin.sh <binary_name>
#   例: ./scripts/run-test-bin.sh shell
#   例: ./scripts/run-test-bin.sh calc

set -e

# 色付き出力用
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m' # No Color

# 引数チェック
BIN_NAME="$1"
if [ -z "$BIN_NAME" ]; then
    echo "Usage: $0 <binary_name>"
    echo "Examples:"
    echo "  $0 shell     # /host/SHELL.ELF を実行"
    echo "  $0 calc      # /host/CALC.ELF を実行"
    echo "  $0 exit0     # /host/EXIT0.ELF を実行"
    exit 1
fi

# FAT32 ファイル名は大文字（8.3 形式）
BIN_UPPER=$(echo "$BIN_NAME" | tr 'a-z' 'A-Z')
BIN_PATH="/host/$BIN_UPPER.ELF"

# 追加の引数があれば渡す
shift
BIN_ARGS="$*"

# プロジェクトルートへ移動
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

# 必要なファイルの存在確認
if [ ! -f "disk.img" ]; then
    echo -e "${RED}ERROR: disk.img not found. Run 'make disk-img' first.${NC}"
    exit 1
fi

if [ ! -f "hostfs.img" ]; then
    echo -e "${RED}ERROR: hostfs.img not found. Run 'make hostfs-update' first.${NC}"
    exit 1
fi

# 一時ファイル
LOG_FILE="/tmp/sabos-test-bin-$$.log"
MONITOR_PORT=55583
KEY_DELAY=0.3

# OVMF ファームウェアの検出
OVMF_CODE="${OVMF_CODE:-$(ls /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd 2>/dev/null | head -1)}"
OVMF_VARS="${OVMF_VARS:-$(ls /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd 2>/dev/null | head -1)}"

if [ -z "$OVMF_CODE" ]; then
    echo -e "${RED}ERROR: OVMF not found. Install with: sudo apt-get install ovmf${NC}"
    exit 1
fi

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
echo "Binary: $BIN_PATH"
if [ -n "$BIN_ARGS" ]; then
    echo "Args: $BIN_ARGS"
fi

# QEMU を起動（disk.img + hostfs.img の 2 台構成）
qemu-system-x86_64 \
    -nodefaults \
    -machine q35 \
    -cpu max \
    -vga std \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_VARS" \
    -drive format=raw,file=fat:rw:esp \
    -drive if=virtio,format=raw,file=disk.img \
    -drive if=virtio,format=raw,file=hostfs.img \
    -netdev user,id=net0,ipv4=on,ipv6=on -device virtio-net-pci,netdev=net0 \
    -audiodev id=snd0,driver=none -device AC97,audiodev=snd0 \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -serial stdio \
    -display none \
    -monitor telnet:127.0.0.1:$MONITOR_PORT,server,nowait > "$LOG_FILE" 2>&1 &

QEMU_PID=$!
echo "QEMU PID: $QEMU_PID"

# --- ヘルパー関数 ---

send_key() {
    local key="$1"
    echo "sendkey $key" | nc -q 1 127.0.0.1 $MONITOR_PORT > /dev/null 2>&1 || true
    sleep "$KEY_DELAY"
}

send_string() {
    local s="$1"
    local i ch lower
    for ((i = 0; i < ${#s}; i++)); do
        ch="${s:i:1}"
        case "$ch" in
            ' ') send_key spc ;;
            '/') send_key slash ;;
            '.') send_key dot ;;
            '-') send_key minus ;;
            '_') send_key shift-minus ;;
            [A-Z])
                lower=$(echo "$ch" | tr 'A-Z' 'a-z')
                send_key "shift-$lower"
                ;;
            *) send_key "$ch" ;;
        esac
    done
}

send_command() {
    local cmd="$1"
    send_string "$cmd"
    send_key ret
}

log_line_count() {
    wc -l < "$LOG_FILE"
}

grep_after() {
    local base="$1"
    local pattern="$2"
    tail -n +"$((base + 1))" "$LOG_FILE" | grep -q "$pattern"
}

wait_for_prompt_after() {
    local base="$1"
    local timeout="${2:-20}"
    for i in $(seq 1 "$timeout"); do
        if grep_after "$base" "user>"; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# --- メイン処理 ---

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

# init のログが落ち着くまで待つ
for i in {1..30}; do
    if grep -q "Entering supervisor loop" "$LOG_FILE" 2>/dev/null; then
        break
    fi
    sleep 1
done

# 空行を送ってプロンプトを揃える
send_key ret
sleep 1

# バイナリを実行
base=$(log_line_count)
echo "Running: run $BIN_PATH $BIN_ARGS"
if [ -n "$BIN_ARGS" ]; then
    send_command "run $BIN_PATH $BIN_ARGS"
else
    send_command "run $BIN_PATH"
fi

# プロセス完了を待つ（最大 60 秒）
# プロセスが終了すると user> プロンプトが再表示される
echo "Waiting for process to complete..."
process_done=false
for i in {1..60}; do
    # プロンプトが戻ったら完了
    if grep_after "$base" "user>"; then
        process_done=true
        break
    fi
    # ページフォルトの検出
    if grep_after "$base" "PAGE FAULT"; then
        echo -e "${RED}ERROR: Process caused a PAGE FAULT${NC}"
        break
    fi
    # カーネルパニックの検出
    if grep_after "$base" "KERNEL PANIC"; then
        echo -e "${RED}ERROR: KERNEL PANIC detected${NC}"
        break
    fi
    sleep 1
done

# QEMU を終了
sleep 1
kill "$QEMU_PID" 2>/dev/null || true
wait "$QEMU_PID" 2>/dev/null || true
QEMU_PID=""

# 実行結果を表示
echo ""
echo "========== PROGRAM OUTPUT =========="
# base 行以降のログからプロンプト行を除いてプログラム出力を表示
tail -n +"$((base + 1))" "$LOG_FILE" | grep -v "^$" | head -200
echo "====================================="
echo ""

if [ "$process_done" = true ]; then
    echo -e "${GREEN}Process completed.${NC}"

    # JSON 出力があればパースして表示（テストバイナリが JSON 結果を出力する場合）
    if grep_after "$base" "SELFTEST JSON"; then
        echo ""
        echo "Structured test results detected:"
        # シリアル出力は \r\n を使うため、tr -d '\r' で除去してからパースする
        json_line=$(tail -n +"$((base + 1))" "$LOG_FILE" | grep "SELFTEST JSON" | tr -d '\r' | sed 's/.*SELFTEST JSON //' | sed 's/ ===//')
        if command -v python3 &>/dev/null && [ -n "$json_line" ]; then
            echo "$json_line" | python3 -c "
import sys, json
data = json.load(sys.stdin)
total = data['total']
passed = data['passed']
failed = data['failed']
print(f'Total: {total}, Passed: {passed}, Failed: {failed}')
if failed > 0:
    print('Failed tests:')
    for r in data['results']:
        if not r['pass']:
            print(f'  - {r[\"name\"]}')
" 2>/dev/null || echo "$json_line"
        else
            echo "$json_line"
        fi
    fi

    exit 0
else
    echo -e "${RED}Process did not complete within timeout.${NC}"
    echo ""
    echo "Full log:"
    cat "$LOG_FILE"
    exit 1
fi
