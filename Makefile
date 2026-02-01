.PHONY: build run run-gui clean

KERNEL_EFI = kernel/target/x86_64-unknown-uefi/debug/sabos.efi
ESP_DIR = esp/EFI/BOOT

# Find OVMF firmware from nix store (installed via devbox)
OVMF_DIR := $(shell nix build nixpkgs\#OVMF.fd --print-out-paths --no-link 2>/dev/null)/FV

build:
	cd kernel && cargo build

$(ESP_DIR):
	mkdir -p $(ESP_DIR)

run: build $(ESP_DIR)
	cp $(KERNEL_EFI) $(ESP_DIR)/BOOTX64.EFI
	qemu-system-x86_64 \
		-nodefaults \
		-machine q35 \
		-vga std \
		-drive if=pflash,format=raw,readonly=on,file=$(OVMF_DIR)/OVMF_CODE.fd \
		-drive if=pflash,format=raw,readonly=on,file=$(OVMF_DIR)/OVMF_VARS.fd \
		-drive format=raw,file=fat:rw:esp \
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

clean:
	cd kernel && cargo clean
	rm -rf esp
