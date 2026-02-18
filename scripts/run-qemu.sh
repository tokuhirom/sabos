#!/bin/bash
# run-qemu.sh — QEMU の起動・停止・ログ管理を一元化するラッパースクリプト
#
# 使い方:
#   ./scripts/run-qemu.sh [OPTIONS]
#
# オプション:
#   --gui            GUI モード（-display gtk、オーディオ SDL）
#   --serial         シリアル出力モード（デフォルト、-display none、オーディオ none）
#   --monitor PORT   モニターポートを指定（デフォルト: 55582）
#   --bg             バックグラウンド実行（PID とログパスを表示して戻る）
#   --log FILE       ログファイルを指定（デフォルト: ./logs/YYYYMMDD-HHMMSS.$$.log）
#   --telnet-port P  ホスト側 telnet ポートを指定（デフォルト: 12323）
#
# 機能:
#   - 起動前に既存 QEMU プロセスを自動 pkill（モニターポートでマッチ）
#   - ログを ./logs/ に自動保存
#   - trap で終了時クリーンアップ（フォアグラウンドモード時）

set -e

# プロジェクトルートへ移動
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

# --- デフォルト値 ---
MODE="serial"           # serial | gui
MONITOR_PORT=55582
TELNET_HOST_PORT=12323
BG_MODE=false
LOG_FILE=""

# --- OVMF ファームウェア検出 ---
# Makefile と同じロジック: 4M 版を優先、なければ通常版
OVMF_CODE="${OVMF_CODE:-$(ls /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd 2>/dev/null | head -1)}"
OVMF_VARS="${OVMF_VARS:-$(ls /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd 2>/dev/null | head -1)}"

if [ -z "$OVMF_CODE" ] || [ -z "$OVMF_VARS" ]; then
    echo "ERROR: OVMF が見つかりません。sudo apt-get install ovmf を実行してください" >&2
    exit 1
fi

# --- 引数解析 ---
while [ $# -gt 0 ]; do
    case "$1" in
        --gui)
            MODE="gui"
            shift
            ;;
        --serial)
            MODE="serial"
            shift
            ;;
        --monitor)
            MONITOR_PORT="$2"
            shift 2
            ;;
        --bg)
            BG_MODE=true
            shift
            ;;
        --log)
            LOG_FILE="$2"
            shift 2
            ;;
        --telnet-port)
            TELNET_HOST_PORT="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1" >&2
            exit 1
            ;;
    esac
done

# --- ログファイルの決定 ---
# ログは ./logs/ ディレクトリに保存する（/tmp/ は使わない）
mkdir -p logs
if [ -z "$LOG_FILE" ]; then
    LOG_FILE="logs/$(date +%Y%m%d-%H%M%S).$$.log"
fi

# --- 既存 QEMU プロセスの自動終了 ---
# 同じモニターポートを使う QEMU が残っていると起動に失敗するため、
# 事前に自動で kill する。これにより「pkill 忘れ」問題を解消する。
pkill -9 -f "qemu-system-x86_64.*$MONITOR_PORT" 2>/dev/null || true
sleep 0.5

# --- QEMU 共通オプションを組み立てる ---
# Makefile の QEMU_COMMON と同等のオプション。変更時はここを更新する。
build_qemu_args() {
    local args=(
        qemu-system-x86_64
        -nodefaults
        -machine q35
        -m 256
        -cpu max
        -vga std
        -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE"
        -drive "if=pflash,format=raw,readonly=on,file=$OVMF_VARS"
        -drive "format=raw,file=fat:rw:esp"
        -drive "if=virtio,format=raw,file=disk.img"
        -drive "if=virtio,format=raw,file=hostfs.img"
        -netdev "user,id=net0,ipv4=on,ipv6=on,hostfwd=tcp::${TELNET_HOST_PORT}-:2323"
        -device virtio-net-pci,netdev=net0
        -virtfs "local,id=fsdev0,path=.,mount_tag=hostfs9p,security_model=none"
        -device "isa-debug-exit,iobase=0xf4,iosize=0x04"
        -serial stdio
        -monitor "telnet:127.0.0.1:${MONITOR_PORT},server,nowait"
    )

    # モード別のオプション
    if [ "$MODE" = "gui" ]; then
        # GUI モード: ウィンドウ表示、SDL オーディオ
        args+=(-audiodev "id=snd0,driver=sdl" -device "AC97,audiodev=snd0")
    else
        # シリアルモード: ディスプレイなし、オーディオ無効
        args+=(-display none)
        args+=(-audiodev "id=snd0,driver=none" -device "AC97,audiodev=snd0")
    fi

    echo "${args[@]}"
}

QEMU_CMD=$(build_qemu_args)

# --- 起動 ---
if [ "$BG_MODE" = true ]; then
    # バックグラウンド実行: PID とログパスを表示して戻る
    $QEMU_CMD > "$LOG_FILE" 2>&1 &
    QEMU_PID=$!
    echo "QEMU started in background"
    echo "  PID: $QEMU_PID"
    echo "  Log: $LOG_FILE"
    echo "  Monitor: telnet 127.0.0.1 $MONITOR_PORT"
else
    # フォアグラウンド実行: Ctrl+C で終了できるように trap 設定
    cleanup() {
        if [ -n "$QEMU_PID" ] && kill -0 "$QEMU_PID" 2>/dev/null; then
            kill "$QEMU_PID" 2>/dev/null || true
            wait "$QEMU_PID" 2>/dev/null || true
        fi
    }
    trap cleanup EXIT

    if [ "$MODE" = "gui" ]; then
        # GUI モードではログをファイルにも tee する
        $QEMU_CMD 2>&1 | tee "$LOG_FILE" &
        QEMU_PID=$!
        wait "$QEMU_PID" || true
    else
        # シリアルモードではログをファイルにも tee する
        $QEMU_CMD 2>&1 | tee "$LOG_FILE" &
        QEMU_PID=$!
        wait "$QEMU_PID" || true
    fi
fi
