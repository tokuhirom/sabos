// qemu.rs — QEMU 固有の機能（デバッグ用）
//
// QEMU の ISA debug exit デバイスを使って、ゲストからホストに終了コードを伝える。
// テスト自動化で「全テスト PASS → exit 0 相当」をホストに伝搬するために使う。
//
// ISA debug exit デバイスの仕組み:
//   QEMU 起動時に `-device isa-debug-exit,iobase=0xf4,iosize=0x04` を指定する。
//   ゲストが I/O ポート 0xf4 に値 v を書き込むと、QEMU は exit code = (v << 1) | 1 で終了する。
//   つまり:
//     - ゲストが 0 を書き込む → QEMU exit code 1（テスト成功の慣例）
//     - ゲストが 1 を書き込む → QEMU exit code 3（テスト失敗の慣例）
//   exit code 0 は ISA debug exit では返せない（常に奇数になる）。

use x86_64::instructions::port::Port;

/// ISA debug exit デバイスの I/O ポートアドレス。
/// QEMU の `-device isa-debug-exit,iobase=0xf4,iosize=0x04` に対応する。
const DEBUG_EXIT_PORT: u16 = 0xf4;

/// QEMU を ISA debug exit デバイス経由で終了させる。
///
/// `code` はゲスト側の終了コード。QEMU の実際の exit code は `(code << 1) | 1` になる。
/// - `code = 0` → QEMU exit code 1（成功）
/// - `code = 1` → QEMU exit code 3（失敗）
///
/// ISA debug exit デバイスが QEMU に設定されていない場合、この関数は I/O ポートに
/// 書き込むだけで何も起こらない（QEMU は終了しない）。
pub fn debug_exit(code: u32) {
    unsafe {
        let mut port = Port::new(DEBUG_EXIT_PORT);
        port.write(code);
    }
}
