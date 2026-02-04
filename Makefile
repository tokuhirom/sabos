# RUSTUP_TOOLCHAIN 環境変数が設定されていると rust-toolchain.toml が無視されるので、
# Make の子プロセスには渡さないようにする
unexport RUSTUP_TOOLCHAIN

.PHONY: build build-user run run-gui screenshot clean disk-img test

KERNEL_EFI = kernel/target/x86_64-unknown-uefi/debug/sabos.efi
USER_ELF = user/target/x86_64-unknown-none/debug/sabos-user
NETD_ELF = user/target/x86_64-unknown-none/debug/netd
INIT_ELF = user/target/x86_64-unknown-none/debug/init
SHELL_ELF = user/target/x86_64-unknown-none/debug/shell
GUI_ELF = user/target/x86_64-unknown-none/debug/gui
CALC_ELF = user/target/x86_64-unknown-none/debug/calc
ESP_DIR = esp/EFI/BOOT

# OVMF ファームウェアの検出（Ubuntu: /usr/share/OVMF/）
# 環境変数 OVMF_CODE / OVMF_VARS で上書き可能
OVMF_CODE ?= $(firstword $(wildcard /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd))
OVMF_VARS ?= $(firstword $(wildcard /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd))
ifeq ($(OVMF_CODE),)
  $(error OVMF が見つかりません。sudo apt-get install ovmf を実行してください)
endif

# virtio-blk 用のディスクイメージ
DISK_IMG = disk.img

# QEMU の共通オプション
# -drive if=virtio で virtio-blk デバイスとしてディスクイメージを接続する。
# -netdev user + -device virtio-net-pci で virtio-net デバイスを追加する。
# PCI バス上に vendor=0x1AF4 のデバイスとして見える。
QEMU_COMMON = qemu-system-x86_64 \
	-nodefaults \
	-machine q35 \
	-vga std \
	-drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE) \
	-drive if=pflash,format=raw,readonly=on,file=$(OVMF_VARS) \
	-drive format=raw,file=fat:rw:esp \
	-drive if=virtio,format=raw,file=$(DISK_IMG) \
	-netdev user,id=net0 -device virtio-net-pci,netdev=net0

# スクリーンショットの出力先（デフォルト: docs/images/screenshot.png）
SCREENSHOT_OUT ?= docs/images/screenshot.png

# QEMU が起動してからスクリーンショットを撮るまでの待ち時間（秒）
SCREENSHOT_WAIT ?= 6

# ユーザープログラムを先にビルドしてから、カーネルをビルドする。
# カーネルは include_bytes! でユーザー ELF バイナリを埋め込むため、
# ユーザーバイナリが存在しないとカーネルのビルドが失敗する。
build: build-user
	cd kernel && cargo build

# ユーザープログラム (x86_64-unknown-none ELF) のビルド
build-user:
	cd user && cargo build

$(ESP_DIR):
	mkdir -p $(ESP_DIR)

# FAT16 ディスクイメージを作成する。
# 32MB のイメージを dd で作り、mkfs.fat -F 16 で FAT16 フォーマットする。
# mtools (mcopy) でテストファイルを書き込む。
# INIT.ELF, SHELL.ELF, NETD.ELF, GUI.ELF, CALC.ELF を書き込む。
# USER_ELF (旧シェル) は HELLO.ELF としてテスト用に残す。
disk-img: build-user
	dd if=/dev/zero of=$(DISK_IMG) bs=1M count=32
	mkfs.fat -F 16 $(DISK_IMG)
	echo "Hello from FAT16!" > /tmp/hello.txt
	mcopy -i $(DISK_IMG) /tmp/hello.txt ::HELLO.TXT
	mcopy -i $(DISK_IMG) $(USER_ELF) ::HELLO.ELF
	mcopy -i $(DISK_IMG) $(NETD_ELF) ::NETD.ELF
	mcopy -i $(DISK_IMG) $(INIT_ELF) ::INIT.ELF
	mcopy -i $(DISK_IMG) $(SHELL_ELF) ::SHELL.ELF
	mcopy -i $(DISK_IMG) $(GUI_ELF) ::GUI.ELF
	mcopy -i $(DISK_IMG) $(CALC_ELF) ::CALC.ELF
	@echo "Disk image created: $(DISK_IMG)"

run: build $(ESP_DIR) $(DISK_IMG)
	cp $(KERNEL_EFI) $(ESP_DIR)/BOOTX64.EFI
	$(QEMU_COMMON) \
		-serial stdio \
		-display none

run-gui: build $(ESP_DIR) $(DISK_IMG)
	cp $(KERNEL_EFI) $(ESP_DIR)/BOOTX64.EFI
	qemu-system-x86_64 \
		-machine q35 \
		-drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE) \
		-drive if=pflash,format=raw,readonly=on,file=$(OVMF_VARS) \
		-drive format=raw,file=fat:rw:esp \
		-drive if=virtio,format=raw,file=$(DISK_IMG) \
		-netdev user,id=net0 -device virtio-net-pci,netdev=net0 \
		-serial stdio

# スクリーンショットを撮る
# 使い方:
#   make screenshot                                        → docs/images/screenshot.png
#   make screenshot SCREENSHOT_OUT=docs/images/foo.png     → docs/images/foo.png
#   make screenshot SCREENSHOT_WAIT=10                     → 10秒待ってから撮影
screenshot: build $(ESP_DIR) $(DISK_IMG)
	cp $(KERNEL_EFI) $(ESP_DIR)/BOOTX64.EFI
	@mkdir -p $(dir $(SCREENSHOT_OUT))
	@echo "Starting QEMU for screenshot..."
	@$(QEMU_COMMON) \
		-serial file:/dev/null \
		-display vnc=:97 \
		-monitor telnet:127.0.0.1:55557,server,nowait & \
	QEMU_PID=$$!; \
	sleep $(SCREENSHOT_WAIT); \
	echo "screendump /tmp/sabos-screenshot.ppm" | nc -q 1 127.0.0.1 55557 > /dev/null 2>&1; \
	sleep 1; \
	kill $$QEMU_PID 2>/dev/null; \
	wait $$QEMU_PID 2>/dev/null || true; \
	convert /tmp/sabos-screenshot.ppm $(SCREENSHOT_OUT); \
	echo "Screenshot saved: $(SCREENSHOT_OUT)"

clean:
	cd kernel && cargo clean
	cd user && cargo clean
	rm -rf esp
	rm -f $(DISK_IMG)

# 自動テストを実行する。
# QEMU を起動して selftest コマンドを実行し、結果を検証する。
# CI で使う場合はこのターゲットを呼ぶ。
test: build $(ESP_DIR) disk-img
	cp $(KERNEL_EFI) $(ESP_DIR)/BOOTX64.EFI
	./scripts/run-selftest.sh
