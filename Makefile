# RUSTUP_TOOLCHAIN 環境変数が設定されていると rust-toolchain.toml が無視されるので、
# Make の子プロセスには渡さないようにする
unexport RUSTUP_TOOLCHAIN

# rust-toolchain.toml から nightly チャンネル名を取得する。
# build-user-std で RUSTUP_TOOLCHAIN を明示的に指定するために使う。
# -Zjson-target-spec は nightly 専用フラグのため、toolchain 指定が必須。
NIGHTLY_CHANNEL := $(shell grep 'channel' rust-toolchain.toml | sed 's/.*= *"\(.*\)"/\1/')

.PHONY: build build-user build-user-std patch-sysroot run run-gui screenshot clean disk-img disk-img-force hostfs-update test test-bin check-syscall

KERNEL_EFI = kernel/target/x86_64-unknown-uefi/debug/sabos.efi
USER_ELF = user/target/x86_64-unknown-none/debug/sabos-user
INIT_ELF = user/target/x86_64-unknown-none/debug/init
SHELL_ELF = user/target/x86_64-unknown-none/debug/shell
GUI_ELF = user/target/x86_64-unknown-none/debug/gui
CALC_ELF = user/target/x86_64-unknown-none/debug/calc
PAD_ELF = user/target/x86_64-unknown-none/debug/pad
TETRIS_ELF = user/target/x86_64-unknown-none/debug/tetris
ED_ELF = user/target/x86_64-unknown-none/debug/ed
HTTPD_ELF = user/target/x86_64-unknown-none/debug/httpd
TELNETD_ELF = user/target/x86_64-unknown-none/debug/telnetd
TSH_ELF = user/target/x86_64-unknown-none/debug/tsh
EXIT0_ELF = user/target/x86_64-unknown-none/debug/exit0
TERM_ELF = user/target/x86_64-unknown-none/debug/term
LIFE_ELF = user/target/x86_64-unknown-none/debug/life
MANDELBROT_ELF = user/target/x86_64-unknown-none/debug/mandelbrot
SNAKE_ELF = user/target/x86_64-unknown-none/debug/snake
SELFTEST_NET_ELF = user/target/x86_64-unknown-none/debug/selftest_net
HELLO_STD_ELF = user-std/target/x86_64-sabos/release/sabos-user-std
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

# ホスト共有用ディスクイメージ（2台目の virtio-blk）
# ユーザーランドバイナリの開発・テスト用。
# disk.img とは別に管理し、mcopy -o でインクリメンタルに更新する。
# ゲスト内からは "/host" としてマウントされる。
HOSTFS_IMG = hostfs.img

# QEMU の共通オプション
# -drive if=virtio で virtio-blk デバイスとしてディスクイメージを接続する。
# 1台目: disk.img（システムディスク）、2台目: hostfs.img（ホスト共有用）
# -netdev user + -device virtio-net-pci で virtio-net デバイスを追加する。
# PCI バス上に vendor=0x1AF4 のデバイスとして見える。
QEMU_COMMON = qemu-system-x86_64 \
	-nodefaults \
	-machine q35 \
	-m 256 \
	-cpu max \
	-vga std \
	-drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE) \
	-drive if=pflash,format=raw,readonly=on,file=$(OVMF_VARS) \
	-drive format=raw,file=fat:rw:esp \
	-drive if=virtio,format=raw,file=$(DISK_IMG) \
	-drive if=virtio,format=raw,file=$(HOSTFS_IMG) \
	-netdev user,id=net0,ipv4=on,ipv6=on,hostfwd=tcp::12323-:2323 -device virtio-net-pci,netdev=net0 \
	-audiodev id=snd0,driver=none -device AC97,audiodev=snd0 \
	-virtfs local,id=fsdev0,path=.,mount_tag=hostfs9p,security_model=none \
	-device isa-debug-exit,iobase=0xf4,iosize=0x04

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

# sysroot にSABOS 用パッチを適用する（idempotent）
patch-sysroot:
	bash scripts/patch-rust-sysroot.sh

# std 対応ユーザープログラムのビルド（要: patch-sysroot 済み）
# RUSTUP_TOOLCHAIN=nightly と -Zjson-target-spec が必要。
# release ビルドにすることでバイナリサイズを大幅に削減する（6.4MB → 29KB）。
# debug ビルドではカーネルヒープの OOM が発生するため release 必須。
#
# sysroot パッチ（rust-std-sabos/ 内のファイル）が変更された場合、
# ビルド済みの std .rlib が古くなるため cargo clean が必要。
# パッチファイルのハッシュを .sysroot-hash に記録して、
# 変更を検出したら自動で cargo clean する。
SYSROOT_HASH_FILE = user-std/.sysroot-hash
build-user-std: patch-sysroot
	@NEW_HASH=$$(find rust-std-sabos/ -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1); \
	OLD_HASH=$$(cat $(SYSROOT_HASH_FILE) 2>/dev/null || echo ""); \
	if [ "$$NEW_HASH" != "$$OLD_HASH" ]; then \
		echo "sysroot パッチが変更されたため cargo clean を実行..."; \
		cd user-std && cargo clean; \
		cd ..; \
		echo "$$NEW_HASH" > $(SYSROOT_HASH_FILE); \
	fi
	cd user-std && RUSTC_BOOTSTRAP_SYNTHETIC_TARGET=1 rustup run $(NIGHTLY_CHANNEL) cargo -Zjson-target-spec build --release

$(ESP_DIR):
	mkdir -p $(ESP_DIR)

# FAT32 ディスクイメージを作成する（ファイルターゲット）。
# disk.img が存在しない場合のみ作成される。
# 64MB のイメージを dd で作り、mkfs.fat -F 32 で FAT32 フォーマットする。
# mtools (mcopy) でテストファイルを書き込む。
# INIT.ELF, SHELL.ELF, GUI.ELF, CALC.ELF, PAD.ELF, TETRIS.ELF, ED.ELF, HTTPD.ELF, TELNETD.ELF, TSH.ELF, EXIT0.ELF, TERM.ELF, LIFE.ELF, MANDEL.ELF を書き込む。
# USER_ELF (旧シェル) は現在は disk.img に含めない。
# order-only 依存（| build-user）にすることで、disk.img が既に存在すれば
# build-user が更新されても再作成しない。明示的に再作成したい場合は make disk-img。
$(DISK_IMG): | build-user
	dd if=/dev/zero of=$(DISK_IMG) bs=1M count=64
	mkfs.fat -F 32 $(DISK_IMG)
	mkdir -p logs
	echo "Hello from FAT32!" > logs/hello.txt
	mcopy -i $(DISK_IMG) logs/hello.txt ::HELLO.TXT
	rm -f logs/hello.txt
	mcopy -i $(DISK_IMG) $(INIT_ELF) ::INIT.ELF
	mcopy -i $(DISK_IMG) $(SHELL_ELF) ::SHELL.ELF
	mcopy -i $(DISK_IMG) $(GUI_ELF) ::GUI.ELF
	mcopy -i $(DISK_IMG) $(CALC_ELF) ::CALC.ELF
	mcopy -i $(DISK_IMG) $(PAD_ELF) ::PAD.ELF
	mcopy -i $(DISK_IMG) $(TETRIS_ELF) ::TETRIS.ELF
	mcopy -i $(DISK_IMG) $(ED_ELF) ::ED.ELF
	mcopy -i $(DISK_IMG) $(HTTPD_ELF) ::HTTPD.ELF
	mcopy -i $(DISK_IMG) $(TELNETD_ELF) ::TELNETD.ELF
	mcopy -i $(DISK_IMG) $(TSH_ELF) ::TSH.ELF
	mcopy -i $(DISK_IMG) $(EXIT0_ELF) ::EXIT0.ELF
	mcopy -i $(DISK_IMG) $(TERM_ELF) ::TERM.ELF
	mcopy -i $(DISK_IMG) $(LIFE_ELF) ::LIFE.ELF
	mcopy -i $(DISK_IMG) $(MANDELBROT_ELF) ::MANDEL.ELF
	mcopy -i $(DISK_IMG) $(SNAKE_ELF) ::SNAKE.ELF
	@# std 対応バイナリがビルド済みならディスクに追加
	@if [ -f "$(HELLO_STD_ELF)" ]; then \
		mcopy -i $(DISK_IMG) $(HELLO_STD_ELF) ::HELLOSTD.ELF; \
		echo "Added HELLOSTD.ELF to disk image"; \
	fi
	@echo "Disk image created: $(DISK_IMG)"

# disk.img を強制的に再作成する（.PHONY な disk-img は後方互換用）
disk-img: build-user
	rm -f $(DISK_IMG)
	$(MAKE) $(DISK_IMG)

# disk-img-force は disk-img のエイリアス
disk-img-force: disk-img

# ホスト共有用ディスクイメージを作成する（初回のみ）。
# 64MB FAT32 としてフォーマットする。
$(HOSTFS_IMG):
	dd if=/dev/zero of=$@ bs=1M count=64
	mkfs.fat -F 32 $@
	@echo "Host filesystem image created: $@"

# ホスト共有用ディスクイメージにユーザーバイナリをコピーする。
# mcopy -o（上書きモード）で変更されたバイナリだけを高速に更新する。
# disk.img の再作成（dd + mkfs.fat）は不要。
# 使い方: make hostfs-update → QEMU 再起動 → /host/SHELL.ELF 等でアクセス
hostfs-update: build-user | $(HOSTFS_IMG)
	mcopy -o -i $(HOSTFS_IMG) $(INIT_ELF) ::INIT.ELF
	mcopy -o -i $(HOSTFS_IMG) $(SHELL_ELF) ::SHELL.ELF
	mcopy -o -i $(HOSTFS_IMG) $(GUI_ELF) ::GUI.ELF
	mcopy -o -i $(HOSTFS_IMG) $(CALC_ELF) ::CALC.ELF
	mcopy -o -i $(HOSTFS_IMG) $(PAD_ELF) ::PAD.ELF
	mcopy -o -i $(HOSTFS_IMG) $(TETRIS_ELF) ::TETRIS.ELF
	mcopy -o -i $(HOSTFS_IMG) $(ED_ELF) ::ED.ELF
	mcopy -o -i $(HOSTFS_IMG) $(HTTPD_ELF) ::HTTPD.ELF
	mcopy -o -i $(HOSTFS_IMG) $(TELNETD_ELF) ::TELNETD.ELF
	mcopy -o -i $(HOSTFS_IMG) $(TSH_ELF) ::TSH.ELF
	mcopy -o -i $(HOSTFS_IMG) $(EXIT0_ELF) ::EXIT0.ELF
	mcopy -o -i $(HOSTFS_IMG) $(TERM_ELF) ::TERM.ELF
	mcopy -o -i $(HOSTFS_IMG) $(LIFE_ELF) ::LIFE.ELF
	mcopy -o -i $(HOSTFS_IMG) $(MANDELBROT_ELF) ::MANDEL.ELF
	mcopy -o -i $(HOSTFS_IMG) $(SNAKE_ELF) ::SNAKE.ELF
	@if [ -f "$(HELLO_STD_ELF)" ]; then \
		mcopy -o -i $(HOSTFS_IMG) $(HELLO_STD_ELF) ::HELLOSTD.ELF; \
	fi
	@echo "Host filesystem updated: $(HOSTFS_IMG)"

# QEMU を起動する（シリアル出力モード）。
# run-qemu.sh が既存 QEMU の自動 pkill、ログの ./logs/ 保存を担当する。
# disk.img が無ければ自動作成される（ファイルターゲット）。
run: build $(ESP_DIR) $(DISK_IMG) $(HOSTFS_IMG)
	cp $(KERNEL_EFI) $(ESP_DIR)/BOOTX64.EFI
	./scripts/run-qemu.sh --serial

# QEMU を起動する（GUI モード）。
# run-qemu.sh が既存 QEMU の自動 pkill、ログの ./logs/ 保存を担当する。
run-gui: build $(ESP_DIR) $(DISK_IMG) $(HOSTFS_IMG)
	cp $(KERNEL_EFI) $(ESP_DIR)/BOOTX64.EFI
	./scripts/run-qemu.sh --gui

# スクリーンショットを撮る
# 使い方:
#   make screenshot                                        → docs/images/screenshot.png
#   make screenshot SCREENSHOT_OUT=docs/images/foo.png     → docs/images/foo.png
#   make screenshot SCREENSHOT_WAIT=10                     → 10秒待ってから撮影
screenshot: build $(ESP_DIR) $(DISK_IMG) $(HOSTFS_IMG)
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
	cd user-std && cargo clean
	rm -rf esp
	rm -f $(DISK_IMG)
	rm -f $(HOSTFS_IMG)

# PAL ファイルの syscall 番号を検証する。
# libs/sabos-syscall/src/lib.rs を正として、rust-std-sabos/*.rs の番号が一致するかチェック。
check-syscall:
	python3 scripts/check-syscall-numbers.py

# 自動テストを実行する。
# QEMU を起動して selftest コマンドを実行し、結果を検証する。
# CI で使う場合はこのターゲットを呼ぶ。
# 自動テストでは disk-img（PHONY）で毎回再作成する。
# テスト対象のバイナリが最新であることを保証するため。
test: check-syscall build build-user-std $(ESP_DIR) disk-img $(HOSTFS_IMG)
	cp $(KERNEL_EFI) $(ESP_DIR)/BOOTX64.EFI
	./scripts/run-selftest.sh

# 特定のユーザーバイナリを /host/ 経由でテスト実行する。
# disk.img の再作成をスキップし、hostfs.img のみインクリメンタル更新する。
# 使い方: make test-bin BIN=shell
# 引数付き: make test-bin BIN=shell BIN_ARGS="arg1 arg2"
test-bin: build hostfs-update $(ESP_DIR)
	@if [ ! -f "$(DISK_IMG)" ]; then echo "Error: $(DISK_IMG) not found. Run 'make disk-img' first."; exit 1; fi
	cp $(KERNEL_EFI) $(ESP_DIR)/BOOTX64.EFI
	./scripts/run-test-bin.sh $(BIN) $(BIN_ARGS)
