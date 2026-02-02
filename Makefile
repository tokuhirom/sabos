.PHONY: build build-user run run-gui screenshot clean

KERNEL_EFI = kernel/target/x86_64-unknown-uefi/debug/sabos.efi
USER_ELF = user/target/x86_64-unknown-none/debug/sabos-user
ESP_DIR = esp/EFI/BOOT

# Find OVMF firmware from nix store (installed via devbox)
OVMF_DIR := $(shell nix build nixpkgs\#OVMF.fd --print-out-paths --no-link 2>/dev/null)/FV

# QEMU の共通オプション
QEMU_COMMON = qemu-system-x86_64 \
	-nodefaults \
	-machine q35 \
	-vga std \
	-drive if=pflash,format=raw,readonly=on,file=$(OVMF_DIR)/OVMF_CODE.fd \
	-drive if=pflash,format=raw,readonly=on,file=$(OVMF_DIR)/OVMF_VARS.fd \
	-drive format=raw,file=fat:rw:esp

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

run: build $(ESP_DIR)
	cp $(KERNEL_EFI) $(ESP_DIR)/BOOTX64.EFI
	$(QEMU_COMMON) \
		-serial stdio \
		-display none

run-gui: build $(ESP_DIR)
	cp $(KERNEL_EFI) $(ESP_DIR)/BOOTX64.EFI
	qemu-system-x86_64 \
		-machine q35 \
		-drive if=pflash,format=raw,readonly=on,file=$(OVMF_DIR)/OVMF_CODE.fd \
		-drive if=pflash,format=raw,readonly=on,file=$(OVMF_DIR)/OVMF_VARS.fd \
		-drive format=raw,file=fat:rw:esp \
		-serial stdio

# スクリーンショットを撮る
# 使い方:
#   make screenshot                                        → docs/images/screenshot.png
#   make screenshot SCREENSHOT_OUT=docs/images/foo.png     → docs/images/foo.png
#   make screenshot SCREENSHOT_WAIT=10                     → 10秒待ってから撮影
screenshot: build $(ESP_DIR)
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
	magick /tmp/sabos-screenshot.ppm $(SCREENSHOT_OUT); \
	echo "Screenshot saved: $(SCREENSHOT_OUT)"

clean:
	cd kernel && cargo clean
	cd user && cargo clean
	rm -rf esp
