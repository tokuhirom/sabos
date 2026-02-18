#!/bin/bash
# run-selftest.sh — SABOS の自動テストを実行するスクリプト
#
# 二段構成:
#   前半: telnet (expect) 経由でユーザーランドテストを実行（信頼性が高い）
#   後半: sendkey 経由でカーネル selftest を実行（ISA debug exit と密結合）
#
# CI での利用を想定しており、全テスト PASS なら終了コード 0、
# 1 つでも FAIL なら終了コード 1 を返す。
#
# ログは ./logs/ に自動保存される（/tmp/ は使わない）。

set -e

# 色付き出力用
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m' # No Color

# プロジェクトルートへ移動
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

# ログディレクトリを作成（./logs/ に保存。/tmp/ は使わない）
mkdir -p logs
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
LOG_FILE="logs/selftest-${TIMESTAMP}.$$.log"
TELNET_LOG="logs/selftest-telnet-${TIMESTAMP}.$$.log"

MONITOR_PORT=55582
TELNET_HOST_PORT=12323
KEY_DELAY=0.3
GUI_SCREENSHOT_PATH_FILE="scripts/gui-screenshot-path.txt"

# クリーンアップ関数
# ログファイルは ./logs/ に永続化するため削除しない
cleanup() {
    if [ -n "$QEMU_PID" ] && kill -0 "$QEMU_PID" 2>/dev/null; then
        kill "$QEMU_PID" 2>/dev/null || true
        wait "$QEMU_PID" 2>/dev/null || true
    fi
    # telnet のテンポラリログだけ削除（メインログは残す）
    rm -f "$TELNET_LOG"
}
trap cleanup EXIT

# 既存の QEMU プロセスを終了（run-qemu.sh と同じロジック）
# モニターポートでマッチさせる
pkill -9 -f "qemu-system-x86_64.*$MONITOR_PORT" 2>/dev/null || true
sleep 1

# EFI をコピー
cp kernel/target/x86_64-unknown-uefi/debug/sabos.efi esp/EFI/BOOT/BOOTX64.EFI

echo "Starting QEMU..."
echo "Log file: $LOG_FILE"

# QEMU を run-qemu.sh 経由で起動（--bg でバックグラウンド実行）
# run-qemu.sh が pkill・ログ管理を担当するが、selftest では独自に QEMU を管理する必要がある
# （PID の取得、exit code の検査など）ため、直接起動する。
# ただし QEMU オプションは run-qemu.sh の build_qemu_args() と同じものを使う。

# OVMF ファームウェアの検出
OVMF_CODE="${OVMF_CODE:-$(ls /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd 2>/dev/null | head -1)}"
OVMF_VARS="${OVMF_VARS:-$(ls /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd 2>/dev/null | head -1)}"

qemu-system-x86_64 \
    -nodefaults \
    -machine q35 \
    -m 256 \
    -cpu max \
    -vga std \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_VARS" \
    -drive format=raw,file=fat:rw:esp \
    -drive if=virtio,format=raw,file=disk.img \
    -drive if=virtio,format=raw,file=hostfs.img \
    -netdev user,id=net0,ipv4=on,ipv6=on,hostfwd=tcp::$TELNET_HOST_PORT-:2323 -device virtio-net-pci,netdev=net0 \
    -audiodev id=snd0,driver=none -device AC97,audiodev=snd0 \
    -virtfs local,id=fsdev0,path=.,mount_tag=hostfs9p,security_model=none \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
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
    echo "Full log saved: $LOG_FILE"
    cat "$LOG_FILE"
    exit 1
fi

echo "Waiting for init supervisor loop..."
for i in {1..30}; do
    if grep -q "Entering supervisor loop" "$LOG_FILE" 2>/dev/null; then
        break
    fi
    sleep 1
done

# =================================================================
# 前半: telnet (expect) 経由でユーザーランドテストを実行
# =================================================================
# sendkey はキードロップが発生し信頼性が低いため、
# ユーザーランドテストは telnet 経由で実行する。

echo ""
echo "========== USERLAND TESTS (via telnet) =========="

# telnetd が起動するまで待つ（telnet 接続が成功するまでリトライ）
echo "Waiting for telnetd to be ready..."
telnet_ready=false
for i in {1..30}; do
    if echo "" | nc -q 1 127.0.0.1 $TELNET_HOST_PORT > /dev/null 2>&1; then
        telnet_ready=true
        break
    fi
    sleep 1
done

if [ "$telnet_ready" != true ]; then
    echo -e "${YELLOW}WARN: telnetd not ready after 30 seconds, skipping userland tests${NC}"
else
    # expect スクリプトでユーザーランドテストを実行する。
    # telnet 経由でコマンドを送信し、出力を検証する。
    # expect はタイムアウト付きのパターンマッチングを提供する。
    EXPECT_TIMEOUT=60

    userland_failed=false

    expect_run() {
        local description="$1"
        local expect_script="$2"
        echo -n "  Testing $description... "
        if expect -c "$expect_script" > "$TELNET_LOG" 2>&1; then
            echo -e "${GREEN}PASSED${NC}"
            return 0
        else
            echo -e "${RED}FAILED${NC}"
            echo "    expect output:"
            sed 's/^/    /' "$TELNET_LOG"
            return 1
        fi
    }

    # --- mkdir/rmdir テスト ---
    expect_run "mkdir/rmdir" "
        set timeout $EXPECT_TIMEOUT
        spawn nc 127.0.0.1 $TELNET_HOST_PORT
        expect \"tsh>\"
        send \"mkdir t\r\"
        expect {
            \"Directory created successfully\" { }
            timeout { exit 1 }
        }
        expect \"tsh>\"
        send \"rmdir t\r\"
        expect {
            \"Directory removed successfully\" { }
            timeout { exit 1 }
        }
        expect \"tsh>\"
        send \"exit\r\"
        expect eof
    " || userland_failed=true

    # --- ls テスト ---
    expect_run "ls" "
        set timeout $EXPECT_TIMEOUT
        spawn nc 127.0.0.1 $TELNET_HOST_PORT
        expect \"tsh>\"
        send \"ls\r\"
        expect {
            \"HELLO.TXT\" { }
            timeout { exit 1 }
        }
        expect \"tsh>\"
        send \"exit\r\"
        expect eof
    " || userland_failed=true

    # --- selftest_net 実行（独立バイナリ） ---
    expect_run "selftest_net" "
        set timeout 120
        spawn nc 127.0.0.1 $TELNET_HOST_PORT
        expect \"tsh>\"
        send \"run /9p/user/target/x86_64-unknown-none/debug/selftest_net\r\"
        expect {
            -re \"NET SELFTEST END:.*PASSED\" { }
            \"NET SELFTEST END\" { exit 1 }
            timeout { exit 1 }
        }
        expect \"tsh>\"
        send \"exit\r\"
        expect eof
    " || userland_failed=true

    # --- HELLOSTD.ELF 実行（std 対応バイナリ） ---
    expect_run "hellostd (std binary)" "
        set timeout 120
        spawn nc 127.0.0.1 $TELNET_HOST_PORT
        expect \"tsh>\"
        send \"run /9p/user-std/target/x86_64-sabos/release/sabos-user-std\r\"
        expect {
            \"Hello from SABOS std\" { }
            timeout { exit 1 }
        }
        # std バイナリの各テスト結果を検証する
        expect {
            \"serde::from_str OK\" { }
            timeout { exit 1 }
        }
        expect \"tsh>\"
        send \"exit\r\"
        expect eof
    " || userland_failed=true

    # --- grep コマンドのテスト ---
    # grep は shell.rs のビルトインなのでここでは cat 経由で確認
    expect_run "cat HELLO.TXT" "
        set timeout $EXPECT_TIMEOUT
        spawn nc 127.0.0.1 $TELNET_HOST_PORT
        expect \"tsh>\"
        send \"cat HELLO.TXT\r\"
        expect {
            \"Hello from FAT32\" { }
            timeout { exit 1 }
        }
        expect \"tsh>\"
        send \"exit\r\"
        expect eof
    " || userland_failed=true

    if [ "$userland_failed" = true ]; then
        echo -e "${RED}Some userland tests FAILED${NC}"
    else
        echo -e "${GREEN}All userland tests PASSED${NC}"
    fi
fi

echo ""
echo "========== KERNEL SELFTEST (via sendkey) =========="

# =================================================================
# 後半: sendkey 経由でカーネル selftest を実行
# =================================================================

# sendkey ヘルパー関数
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
            # 大文字は shift- 付きで送信する（QEMU sendkey は小文字キー名のみ受付）
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
    for i in {1..20}; do
        if grep_after "$base" "user>"; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# init のログが落ち着いたタイミングで空行を送ってプロンプトを揃える
echo "sendkey ret" | nc -q 1 127.0.0.1 $MONITOR_PORT > /dev/null 2>&1 || true
sleep 0.5

# user> プロンプトが再表示されるまで待つ
for i in {1..10}; do
    if grep -q "user>" "$LOG_FILE" 2>/dev/null; then
        break
    fi
    sleep 1
done

# GUI アプリのスクリーンショット（任意）
if [ -f "$GUI_SCREENSHOT_PATH_FILE" ]; then
    GUI_SCREENSHOT_OUT="$(cat "$GUI_SCREENSHOT_PATH_FILE")"
    rm -f "$GUI_SCREENSHOT_PATH_FILE"
    echo "Spawning GUI apps for screenshot..."
    send_command "spawn /CALC.ELF"
    send_command "spawn /PAD.ELF"
    sleep 4
    echo "Capturing GUI screenshot..."
    mkdir -p "$(dirname "$GUI_SCREENSHOT_OUT")"
    # スクリーンショットのテンポラリは logs/ に保存
    echo "screendump logs/sabos-gui-shot.ppm" | nc -q 1 127.0.0.1 $MONITOR_PORT > /dev/null 2>&1 || true
    sleep 1
    convert logs/sabos-gui-shot.ppm "$GUI_SCREENSHOT_OUT"
    rm -f logs/sabos-gui-shot.ppm
    echo "GUI screenshot saved: $GUI_SCREENSHOT_OUT"
fi

echo "Sending selftest command..."

# selftest 送信前にシステムを安定させる。
sleep 2
send_key ret
sleep 1

# user シェルで selftest --exit を実行する。
# --exit フラグは syscall 経由でカーネルに渡され、ISA debug exit で QEMU を自動終了する:
#   全テスト PASS → QEMU exit 1（ゲストが 0 を書き込む → (0 << 1) | 1 = 1）
#   テスト FAIL あり → QEMU exit 3（ゲストが 1 を書き込む → (1 << 1) | 1 = 3）
#
# sendkey でキーが欠落することがあるため、最大 2 回リトライする。
selftest_started=false
for attempt in 1 2; do
    base=$(log_line_count)
    if [ -n "${SELFTEST_TARGET:-}" ]; then
        send_command "selftest ${SELFTEST_TARGET} --exit"
    else
        send_command "selftest --exit"
    fi

    # テスト開始の反応を待つ（最大 15 秒）
    for i in {1..15}; do
        if grep_after "$base" "Running kernel selftest" || grep_after "$base" "SELFTEST START"; then
            selftest_started=true
            break
        fi
        sleep 1
    done

    if [ "$selftest_started" = true ]; then
        break
    fi

    echo "selftest did not start (attempt $attempt), retrying..."
    # 反応がない場合は改行を送ってプロンプトをリセットしてからリトライ
    send_key ret
    sleep 2
done

if [ "$selftest_started" != true ]; then
    echo -e "${RED}ERROR: selftest did not start after retries${NC}"
    echo "Full log saved: $LOG_FILE"
    cat "$LOG_FILE"
    exit 1
fi

# QEMU が ISA debug exit で自動終了するのを待つ（最大 180 秒）
# selftest --exit は完了後に I/O ポート 0xf4 に書き込み、QEMU を終了させる。
# sendkey で --exit フラグが欠落することがあるため、タイムアウト付きで待つ。
echo "Waiting for selftest to complete (QEMU will auto-exit)..."
qemu_exit=0
WAIT_TIMEOUT=180
for i in $(seq 1 $WAIT_TIMEOUT); do
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
        # QEMU が終了した
        break
    fi
    # SELFTEST END が出力されたかチェック（--exit が欠落した場合の検出）
    if [ $((i % 10)) -eq 0 ] && grep -q "^=== SELFTEST END:" "$LOG_FILE" 2>/dev/null; then
        echo "SELFTEST END detected but QEMU still running (--exit flag likely lost)"
        echo "Killing QEMU and parsing results from log..."
        kill "$QEMU_PID" 2>/dev/null || true
        sleep 1
        break
    fi
    sleep 1
done
# タイムアウト後も QEMU が生きていたら強制終了
if kill -0 "$QEMU_PID" 2>/dev/null; then
    echo "QEMU still running after ${WAIT_TIMEOUT}s timeout, killing..."
    kill "$QEMU_PID" 2>/dev/null || true
    sleep 1
fi
wait "$QEMU_PID" 2>/dev/null || qemu_exit=$?
QEMU_PID=""  # cleanup で再度 kill しないように

# 結果を表示
echo ""
echo "========== SELFTEST OUTPUT =========="
grep -E "(SELFTEST|PASS|FAIL)" "$LOG_FILE" || true
echo "====================================="
echo ""

# ログの保存先を表示（./logs/ に自動保存されている）
echo "QEMU log saved: $LOG_FILE"

# 結果を検証（3 段階のフォールバック）
#   1. QEMU exit code（ISA debug exit 経由: 1=成功, 3=失敗）
#   2. JSON サマリーのパース
#   3. grep フォールバック

echo "QEMU exit code: $qemu_exit"

# JSON サマリーも取得して詳細表示する（exit code に関わらず）
# シリアル出力は \r\n を使うため、tr -d '\r' で除去してからパースする
json_line=$(grep "SELFTEST JSON" "$LOG_FILE" | tail -1 | tr -d '\r' | sed 's/.*SELFTEST JSON //' | sed 's/ ===//' || true)
if [ -n "$json_line" ] && command -v python3 &>/dev/null; then
    json_exit=0
    result=$(echo "$json_line" | python3 -c "
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
sys.exit(1 if failed > 0 else 0)
" 2>&1) || json_exit=$?
    echo "$result"
    echo ""
fi

# ユーザーランドテストの失敗も最終結果に反映する
if [ "${userland_failed:-false}" = true ]; then
    echo -e "${RED}Some userland tests FAILED!${NC}"
    echo ""
    echo "Full log: $LOG_FILE"
    exit 1
fi

# ISA debug exit の exit code で判定（最も信頼性が高い）
# exit code 1 = ゲストが 0 を書き込み = 全テスト PASS
# exit code 3 = ゲストが 1 を書き込み = テスト FAIL あり
# それ以外 = QEMU が異常終了またはタイムアウト → JSON/grep にフォールバック
if [ "$qemu_exit" -eq 1 ]; then
    echo -e "${GREEN}All tests PASSED! (QEMU exit code: $qemu_exit)${NC}"
    exit 0
elif [ "$qemu_exit" -eq 3 ]; then
    echo -e "${RED}Some tests FAILED! (QEMU exit code: $qemu_exit)${NC}"
    echo ""
    echo "Full log: $LOG_FILE"
    exit 1
else
    # ISA debug exit が使えなかった場合（kill されたなど）: JSON / grep にフォールバック
    echo -e "${YELLOW}WARN: Unexpected QEMU exit code: $qemu_exit (falling back to output parsing)${NC}"
    if [ -n "$json_line" ] && [ "${json_exit:-1}" -eq 0 ]; then
        echo -e "${GREEN}All tests PASSED! (from JSON)${NC}"
        exit 0
    elif grep -q "^=== SELFTEST END:.*PASSED ===" "$LOG_FILE"; then
        echo -e "${GREEN}All tests PASSED! (from grep)${NC}"
        exit 0
    else
        echo -e "${RED}Some tests FAILED!${NC}"
        echo ""
        echo "Full log: $LOG_FILE"
        exit 1
    fi
fi
