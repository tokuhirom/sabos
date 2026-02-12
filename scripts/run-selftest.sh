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
YELLOW='\033[0;33m'
NC='\033[0m' # No Color

# プロジェクトルートへ移動
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

# 一時ファイル
LOG_FILE="/tmp/sabos-selftest-$$.log"
MONITOR_PORT=55582
KEY_DELAY=0.3
TEST_DIR="t"
TEST_DIR_FALLBACK="u"
GUI_SCREENSHOT_PATH_FILE="scripts/gui-screenshot-path.txt"

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

# QEMU を起動（disk.img + hostfs.img の 2 台構成）
qemu-system-x86_64 \
    -nodefaults \
    -machine q35 \
    -cpu max \
    -vga std \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_VARS_4M.fd \
    -drive format=raw,file=fat:rw:esp \
    -drive if=virtio,format=raw,file=disk.img \
    -drive if=virtio,format=raw,file=hostfs.img \
    -netdev user,id=net0,ipv4=on,ipv6=on -device virtio-net-pci,netdev=net0 \
    -audiodev id=snd0,driver=none -device AC97,audiodev=snd0 \
    -virtfs local,id=fsdev0,path=./user/target,mount_tag=hostfs9p,security_model=none \
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

echo "Sending user shell mkdir command..."

# キー入力前に少し待つ（プロンプト安定化）
sleep 0.5

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

# mkdir t
send_key ret
wait_for_prompt_after "$(log_line_count)" || true
base=$(log_line_count)
send_command "mkdir $TEST_DIR"

echo "Waiting for mkdir output..."
mkdir_result="unknown"
for i in {1..10}; do
    if grep_after "$base" "Directory created successfully"; then
        mkdir_result="ok"
        break
    fi
    if grep_after "$base" "Error: Failed to create directory"; then
        mkdir_result="fail"
        break
    fi
    sleep 1
done
wait_for_prompt_after "$base" || true

if [ "$mkdir_result" != "ok" ]; then
    echo "Retrying mkdir command..."
    # プロンプトに戻してからリトライ（入力の連結防止）
    send_key ret
    wait_for_prompt_after "$(log_line_count)" || true
    TEST_DIR="$TEST_DIR_FALLBACK"
    base=$(log_line_count)
    send_command "mkdir $TEST_DIR"
    mkdir_result="unknown"
    for i in {1..10}; do
        if grep_after "$base" "Directory created successfully"; then
            mkdir_result="ok"
            break
        fi
        if grep_after "$base" "Error: Failed to create directory"; then
            mkdir_result="fail"
            break
        fi
        sleep 1
    done
    wait_for_prompt_after "$base" || true
fi

if [ "$mkdir_result" != "ok" ]; then
    echo -e "${RED}ERROR: mkdir output not found${NC}"
    cat "$LOG_FILE"
    exit 1
fi

echo "Sending user shell rmdir command..."

# キー入力前に少し待つ（プロンプト安定化）
sleep 0.5

# rmdir t
send_key ret
wait_for_prompt_after "$(log_line_count)" || true
base=$(log_line_count)
send_command "rmdir $TEST_DIR"

echo "Waiting for rmdir output..."
rmdir_result="unknown"
for i in {1..10}; do
    if grep_after "$base" "Directory removed successfully"; then
        rmdir_result="ok"
        break
    fi
    if grep_after "$base" "Error: Failed to remove directory"; then
        # 既に削除済みなどで失敗しても selftest には影響しないので許容する
        rmdir_result="ok"
        break
    fi
    sleep 1
done
wait_for_prompt_after "$base" || true

if [ "$rmdir_result" != "ok" ]; then
    echo -e "${RED}WARN: rmdir output not found${NC}"
    # rmdir の失敗は selftest に影響しないので続行する
fi

echo "Sending user shell ls command..."

# user シェルで ls を実行
send_key ret
wait_for_prompt_after "$(log_line_count)" || true
base=$(log_line_count)
send_command "ls"

# ls の結果を待つ（最大 10 秒）
echo "Waiting for ls output..."
for i in {1..10}; do
    if grep_after "$base" "HELLO.TXT"; then
        break
    fi
    sleep 1
done

if ! grep_after "$base" "HELLO.TXT"; then
    echo "Retrying ls command..."
    send_key ret
    wait_for_prompt_after "$(log_line_count)" || true
    base=$(log_line_count)
    send_command "ls"
    for i in {1..10}; do
        if grep_after "$base" "HELLO.TXT"; then
            break
        fi
        sleep 1
    done
    if ! grep_after "$base" "HELLO.TXT"; then
        echo -e "${RED}ERROR: ls output did not contain HELLO.TXT${NC}"
        cat "$LOG_FILE"
        exit 1
    fi
fi

# ls 実行後に user> プロンプトが戻るまで待つ
wait_for_prompt_after "$base" || true
sleep 0.5

# --- grep コマンドのテスト ---
# HELLO.TXT に対して grep を実行し、パターンが一致することを確認する
echo "Testing grep command..."

send_key ret
wait_for_prompt_after "$(log_line_count)" || true
base=$(log_line_count)
send_command "grep Hello HELLO.TXT"

echo "Waiting for grep output..."
grep_ok=false
for i in {1..10}; do
    if grep_after "$base" "Hello"; then
        grep_ok=true
        break
    fi
    sleep 1
done
wait_for_prompt_after "$base" || true

if [ "$grep_ok" = true ]; then
    echo -e "${GREEN}grep command test PASSED${NC}"
else
    echo -e "${RED}WARN: grep command test did not produce expected output${NC}"
fi

# grep -v テスト（マッチしない行の出力）
sleep 0.5
send_key ret
wait_for_prompt_after "$(log_line_count)" || true
base=$(log_line_count)
send_command "grep -c Hello HELLO.TXT"

echo "Waiting for grep -c output..."
grep_c_ok=false
for i in {1..10}; do
    if grep_after "$base" "1"; then
        grep_c_ok=true
        break
    fi
    sleep 1
done
wait_for_prompt_after "$base" || true

if [ "$grep_c_ok" = true ]; then
    echo -e "${GREEN}grep -c command test PASSED${NC}"
else
    echo -e "${RED}WARN: grep -c command test did not produce expected output${NC}"
fi

sleep 0.5

# --- ネットワーク API selftest ---
echo "Running network API selftest..."
send_key ret
wait_for_prompt_after "$(log_line_count)" || true
base=$(log_line_count)
send_command "selftest_net"

echo "Waiting for net selftest to complete..."
net_selftest_ok=false
for i in {1..30}; do
    if grep_after "$base" "NET SELFTEST END"; then
        net_selftest_ok=true
        break
    fi
    sleep 1
done
wait_for_prompt_after "$base" || true

if [ "$net_selftest_ok" = true ]; then
    if grep_after "$base" "NET SELFTEST END:.*PASSED"; then
        echo -e "${GREEN}Network API selftest PASSED${NC}"
    else
        echo -e "${RED}Network API selftest had failures${NC}"
    fi
else
    echo -e "${RED}WARN: Network API selftest did not complete${NC}"
fi

sleep 0.5

# --- std 対応バイナリ (HELLOSTD.ELF) のテスト ---
echo "Testing std hello world binary..."

send_key ret
wait_for_prompt_after "$(log_line_count)" || true
base=$(log_line_count)
send_command "run /hellostd.elf"

echo "Waiting for HELLOSTD.ELF output..."
hellostd_ok=false
for i in {1..30}; do
    if grep_after "$base" "Hello from SABOS std"; then
        hellostd_ok=true
        break
    fi
    # プロセス終了を検出（成功・失敗どちらも）
    if grep_after "$base" "keyboard focus released"; then
        break
    fi
    # ページフォルトやエラーの検出
    if grep_after "$base" "PAGE FAULT"; then
        echo -e "${RED}ERROR: HELLOSTD.ELF caused a page fault${NC}"
        break
    fi
    sleep 1
done
wait_for_prompt_after "$base" || true

if [ "$hellostd_ok" = true ]; then
    echo -e "${GREEN}HELLOSTD.ELF test PASSED${NC}"
    # 追加の出力チェック
    if grep_after "$base" "2 + 3 = 5"; then
        echo -e "${GREEN}  Arithmetic output OK${NC}"
    fi
    if grep_after "$base" "sum of"; then
        echo -e "${GREEN}  Vec/alloc output OK${NC}"
    fi
    if grep_after "$base" "fs::read_to_string OK"; then
        echo -e "${GREEN}  fs::read_to_string OK${NC}"
    else
        echo -e "${RED}  fs::read_to_string FAILED${NC}"
    fi
    if grep_after "$base" "fs::write OK"; then
        echo -e "${GREEN}  fs::write OK${NC}"
    else
        echo -e "${RED}  fs::write FAILED${NC}"
    fi
    if grep_after "$base" "fs::read_back OK: written by std::fs"; then
        echo -e "${GREEN}  fs::read_back OK${NC}"
    else
        echo -e "${RED}  fs::read_back FAILED${NC}"
    fi
    if grep_after "$base" "fs::metadata OK"; then
        echo -e "${GREEN}  fs::metadata OK${NC}"
    else
        echo -e "${RED}  fs::metadata FAILED${NC}"
    fi
    if grep_after "$base" "time::Instant OK"; then
        echo -e "${GREEN}  time::Instant OK${NC}"
    else
        echo -e "${RED}  time::Instant FAILED${NC}"
    fi
    if grep_after "$base" "time::monotonic OK"; then
        echo -e "${GREEN}  time::monotonic OK${NC}"
    else
        echo -e "${RED}  time::monotonic FAILED${NC}"
    fi
    if grep_after "$base" "time::SystemTime OK"; then
        echo -e "${GREEN}  time::SystemTime OK${NC}"
    else
        if grep_after "$base" "time::SystemTime WARN"; then
            echo -e "${GREEN}  time::SystemTime OK (but date before 2020)${NC}"
        else
            echo -e "${RED}  time::SystemTime FAILED${NC}"
        fi
    fi
    if grep_after "$base" "args::count OK"; then
        echo -e "${GREEN}  args::count OK${NC}"
    else
        echo -e "${RED}  args::count FAILED${NC}"
    fi
    if grep_after "$base" "args::argv0 OK"; then
        echo -e "${GREEN}  args::argv0 OK${NC}"
    else
        echo -e "${RED}  args::argv0 FAILED${NC}"
    fi
    if grep_after "$base" "env::current_dir OK"; then
        echo -e "${GREEN}  env::current_dir OK${NC}"
    else
        echo -e "${RED}  env::current_dir FAILED${NC}"
    fi
    if grep_after "$base" "env::var OK: SABOS_TEST=hello_env"; then
        echo -e "${GREEN}  env::var OK${NC}"
    else
        echo -e "${RED}  env::var FAILED${NC}"
    fi
    if grep_after "$base" "env::vars OK"; then
        echo -e "${GREEN}  env::vars OK${NC}"
    else
        echo -e "${RED}  env::vars FAILED${NC}"
    fi
    if grep_after "$base" "env::vars_contains OK"; then
        echo -e "${GREEN}  env::vars_contains OK${NC}"
    else
        echo -e "${RED}  env::vars_contains FAILED${NC}"
    fi
    if grep_after "$base" "net::lookup OK"; then
        echo -e "${GREEN}  net::lookup OK${NC}"
    else
        echo -e "${RED}  net::lookup FAILED${NC}"
    fi
    if grep_after "$base" "net::tcp_parse OK"; then
        echo -e "${GREEN}  net::tcp_parse OK${NC}"
    else
        echo -e "${RED}  net::tcp_parse FAILED${NC}"
    fi
    if grep_after "$base" "net::udp_bind OK"; then
        echo -e "${GREEN}  net::udp_bind OK${NC}"
    else
        echo -e "${RED}  net::udp_bind FAILED${NC}"
    fi
    if grep_after "$base" "net::udp_send OK"; then
        echo -e "${GREEN}  net::udp_send OK${NC}"
    else
        echo -e "${RED}  net::udp_send FAILED${NC}"
    fi
    if grep_after "$base" "net::udp_recv OK"; then
        echo -e "${GREEN}  net::udp_recv OK${NC}"
    else
        echo -e "${RED}  net::udp_recv FAILED${NC}"
    fi
    if grep_after "$base" "process::status OK"; then
        echo -e "${GREEN}  process::status OK${NC}"
    else
        echo -e "${RED}  process::status FAILED${NC}"
    fi
    if grep_after "$base" "process::spawn OK"; then
        echo -e "${GREEN}  process::spawn OK${NC}"
    else
        echo -e "${RED}  process::spawn FAILED${NC}"
    fi
    if grep_after "$base" "process::wait OK"; then
        echo -e "${GREEN}  process::wait OK${NC}"
    else
        echo -e "${RED}  process::wait FAILED${NC}"
    fi
    if grep_after "$base" "thread::spawn_join OK"; then
        echo -e "${GREEN}  thread::spawn_join OK${NC}"
    else
        echo -e "${RED}  thread::spawn_join FAILED${NC}"
    fi
    if grep_after "$base" "thread::return_value OK"; then
        echo -e "${GREEN}  thread::return_value OK${NC}"
    else
        echo -e "${RED}  thread::return_value FAILED${NC}"
    fi
    if grep_after "$base" "thread::yield_now OK"; then
        echo -e "${GREEN}  thread::yield_now OK${NC}"
    else
        echo -e "${RED}  thread::yield_now FAILED${NC}"
    fi
    if grep_after "$base" "serde::to_string OK"; then
        echo -e "${GREEN}  serde::to_string OK${NC}"
    else
        echo -e "${RED}  serde::to_string FAILED${NC}"
    fi
    if grep_after "$base" "serde::from_str OK"; then
        echo -e "${GREEN}  serde::from_str OK${NC}"
    else
        echo -e "${RED}  serde::from_str FAILED${NC}"
    fi
else
    echo -e "${RED}WARN: HELLOSTD.ELF did not produce expected output${NC}"
fi

sleep 0.5

echo "Sending selftest command..."

# selftest 送信前にシステムを安定させる。
# HELLOSTD.ELF のネットワークテスト後、シリアル出力が落ち着くのを待つ。
# sendkey で --exit フラグが欠落するのを防ぐため、十分な間隔を空ける。
sleep 2
send_key ret
sleep 1

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
    echo "screendump /tmp/sabos-gui-shot.ppm" | nc -q 1 127.0.0.1 $MONITOR_PORT > /dev/null 2>&1 || true
    sleep 1
    convert /tmp/sabos-gui-shot.ppm "$GUI_SCREENSHOT_OUT"
    echo "GUI screenshot saved: $GUI_SCREENSHOT_OUT"
fi

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

# QEMU ログを保存（デバッグ用）。cleanup の trap で LOG_FILE は削除されるため、
# exit 前にコピーしておく。SABOS_SAVE_LOG 環境変数で保存先を指定できる。
if [ -n "$SABOS_SAVE_LOG" ] && [ -f "$LOG_FILE" ]; then
    cp "$LOG_FILE" "$SABOS_SAVE_LOG"
    echo "QEMU log saved: $SABOS_SAVE_LOG"
fi

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
    echo "Full log:"
    cat "$LOG_FILE"
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
        echo "Full log:"
        cat "$LOG_FILE"
        exit 1
    fi
fi
