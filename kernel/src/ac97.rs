// ac97.rs — Intel AC97 オーディオコントローラドライバ
//
// AC97 (Audio Codec '97) は Intel が策定したオーディオコーデック規格。
// QEMU の `-device AC97` でエミュレートされる Intel 82801AA AC97 コントローラを制御する。
//
// AC97 コントローラには 2 つの I/O 空間がある:
//   - NAM (Native Audio Mixer): BAR0 — コーデックのミキサーレジスタ（音量制御等）
//   - NABM (Native Audio Bus Master): BAR1 — DMA 転送制御（再生/録音）
//
// 再生の仕組み:
//   1. PCM データをメモリ上のバッファに書き込む
//   2. BDL (Buffer Descriptor List) にバッファの物理アドレスとサンプル数を設定
//   3. NABM の PCM Out レジスタに BDL のアドレスを書き込み、Run ビットを立てる
//   4. コントローラが DMA で PCM データを読み取り、DAC に送信する
//
// PCM フォーマット: 48kHz, 16-bit signed little-endian, stereo (4 bytes/sample)

use spin::Mutex;
use x86_64::instructions::port::Port;
use core::alloc::Layout;
use crate::serial_println;
use crate::pci;

/// AC97 ドライバのグローバルインスタンス。
/// 他のモジュール（syscall, shell）から AC97::lock() でアクセスする。
pub static AC97: Mutex<Option<Ac97>> = Mutex::new(None);

// =================================================================
// NAM (Native Audio Mixer) レジスタオフセット
// =================================================================
//
// NAM は BAR0 の I/O ポートベースからのオフセット。
// コーデックのミキサー設定（音量、ミュート、サンプルレート等）を制御する。

/// コーデックリセットレジスタ（16bit write でリセット実行）
const NAM_RESET: u16 = 0x00;
/// マスターボリューム（16bit, 0x0000 = 最大音量, 0x8000 = ミュート）
const NAM_MASTER_VOL: u16 = 0x02;
/// PCM 出力ボリューム（16bit, 0x0000 = 最大音量）
const NAM_PCM_OUT_VOL: u16 = 0x18;

// =================================================================
// NABM (Native Audio Bus Master) レジスタオフセット
// =================================================================
//
// NABM は BAR1 の I/O ポートベースからのオフセット。
// DMA エンジンの制御を行う。PCM Out チャンネルは 0x10 から始まる。

/// PCM Out — Buffer Descriptor List Base Address（32bit, BDL の物理アドレス）
const PO_BDBAR: u16 = 0x10;
/// PCM Out — Current Index Value（8bit, 現在再生中のバッファ番号、読み取り専用）
/// 現在は直接使用していないが、デバッグ時に有用なのでコメントとして残す。
// const PO_CIV: u16 = 0x14;
/// PCM Out — Last Valid Index（8bit, 最後の有効な BDL エントリの番号）
const PO_LVI: u16 = 0x15;
/// PCM Out — Status Register（16bit, write-clear でエラー/完了フラグをクリア）
const PO_SR: u16 = 0x16;
/// PCM Out — Control Register（8bit, bit0=Run, bit1=Reset）
const PO_CR: u16 = 0x1B;
/// Global Control（32bit, bit1=Cold Reset Release）
const GLOB_CNT: u16 = 0x2C;
/// Global Status（32bit, bit0=Primary Codec Ready）
const GLOB_STA: u16 = 0x30;

// =================================================================
// BDL (Buffer Descriptor List) エントリ
// =================================================================
//
// BDL は最大 32 エントリの配列。各エントリは 8 バイト。
// DMA エンジンは BDL のエントリを順番に処理し、
// 各バッファの PCM データをオーディオ出力に送る。

/// BDL エントリの構造体（8 バイト、packed）
///
/// - addr: PCM バッファの物理アドレス（32bit）
/// - samples: バッファ内のサンプル数（16bit）
///   ※ サンプル = 1つのステレオペア (左16bit + 右16bit = 4bytes)
/// - flags: 制御フラグ（16bit, bit15 = IOC = Interrupt on Completion）
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct BdlEntry {
    addr: u32,
    samples: u16,
    flags: u16,
}

/// サンプルレート（Hz）— AC97 のデフォルトは 48kHz
const SAMPLE_RATE: u32 = 48000;

/// BDL エントリ数（最大 32、ここでは再生に必要な分だけ使う）
const BDL_ENTRIES: usize = 32;

/// 1 バッファあたりのサンプル数
/// 各サンプル = ステレオ 16bit = 4 bytes
/// 4096 サンプル × 4 bytes = 16384 bytes (16KB)
const SAMPLES_PER_BUF: usize = 4096;

/// 1 バッファあたりのバイト数（サンプル数 × 4 bytes/sample）
const BYTES_PER_BUF: usize = SAMPLES_PER_BUF * 4;

// =================================================================
// 256 エントリの sin ルックアップテーブル（振幅 32000）
// =================================================================
//
// no_std 環境では浮動小数点の sin() が使えないので、
// 整数の固定小数点ルックアップテーブルで正弦波を生成する。
//
// テーブルは 0〜2π を 256 等分した sin 値を振幅 32000 にスケーリングしたもの。
// i16 の最大値は 32767 なので、32000 は十分な音量でクリッピングしない。
//
// 使い方: sin_table[phase >> 8] で sin 値を取得する。
// phase は 0〜65535 の固定小数点角度（上位 8bit がテーブルインデックス）。

static SIN_TABLE: [i16; 256] = [
        0,    785,   1570,   2354,   3137,   3917,   4695,   5471,
     6243,   7011,   7775,   8535,   9289,  10038,  10780,  11517,
    12246,  12968,  13682,  14388,  15085,  15773,  16451,  17120,
    17778,  18426,  19062,  19687,  20301,  20902,  21490,  22065,
    22627,  23176,  23710,  24231,  24736,  25227,  25703,  26163,
    26607,  27035,  27447,  27843,  28221,  28583,  28928,  29255,
    29564,  29856,  30129,  30385,  30622,  30841,  31041,  31222,
    31385,  31529,  31654,  31759,  31846,  31913,  31961,  31990,
    32000,  31990,  31961,  31913,  31846,  31759,  31654,  31529,
    31385,  31222,  31041,  30841,  30622,  30385,  30129,  29856,
    29564,  29255,  28928,  28583,  28221,  27843,  27447,  27035,
    26607,  26163,  25703,  25227,  24736,  24231,  23710,  23176,
    22627,  22065,  21490,  20902,  20301,  19687,  19062,  18426,
    17778,  17120,  16451,  15773,  15085,  14388,  13682,  12968,
    12246,  11517,  10780,  10038,   9289,   8535,   7775,   7011,
     6243,   5471,   4695,   3917,   3137,   2354,   1570,    785,
        0,   -785,  -1570,  -2354,  -3137,  -3917,  -4695,  -5471,
    -6243,  -7011,  -7775,  -8535,  -9289, -10038, -10780, -11517,
   -12246, -12968, -13682, -14388, -15085, -15773, -16451, -17120,
   -17778, -18426, -19062, -19687, -20301, -20902, -21490, -22065,
   -22627, -23176, -23710, -24231, -24736, -25227, -25703, -26163,
   -26607, -27035, -27447, -27843, -28221, -28583, -28928, -29255,
   -29564, -29856, -30129, -30385, -30622, -30841, -31041, -31222,
   -31385, -31529, -31654, -31759, -31846, -31913, -31961, -31990,
   -32000, -31990, -31961, -31913, -31846, -31759, -31654, -31529,
   -31385, -31222, -31041, -30841, -30622, -30385, -30129, -29856,
   -29564, -29255, -28928, -28583, -28221, -27843, -27447, -27035,
   -26607, -26163, -25703, -25227, -24736, -24231, -23710, -23176,
   -22627, -22065, -21490, -20902, -20301, -19687, -19062, -18426,
   -17778, -17120, -16451, -15773, -15085, -14388, -13682, -12968,
   -12246, -11517, -10780, -10038,  -9289,  -8535,  -7775,  -7011,
    -6243,  -5471,  -4695,  -3917,  -3137,  -2354,  -1570,   -785,
];

/// AC97 ドライバの状態を保持する構造体
pub struct Ac97 {
    /// NAM (Native Audio Mixer) のベース I/O ポートアドレス
    /// 将来のミキサー制御（音量変更等）で使用予定
    #[allow(dead_code)]
    nam_base: u16,
    /// NABM (Native Audio Bus Master) のベース I/O ポートアドレス
    nabm_base: u16,
    /// BDL (Buffer Descriptor List) のメモリポインタ
    /// 32 エントリ × 8 bytes = 256 bytes
    bdl_ptr: *mut BdlEntry,
    /// PCM バッファ群の先頭ポインタ
    /// 32 バッファ × 16KB = 512KB
    pcm_buf_ptr: *mut u8,
}

// Ac97 は Mutex で保護するので Send + Sync を実装
unsafe impl Send for Ac97 {}
unsafe impl Sync for Ac97 {}

/// AC97 ドライバを初期化する。
///
/// PCI バスから AC97 デバイスを検出し、BAR を読み取り、
/// コーデックを初期化する。デバイスが見つからない場合は何もしない。
pub fn init() {
    let dev = match pci::find_ac97() {
        Some(d) => d,
        None => {
            serial_println!("AC97: device not found");
            return;
        }
    };

    serial_println!(
        "AC97: found at bus={}, dev={}, func={} (vendor={:#06x}, device={:#06x})",
        dev.bus, dev.device, dev.function, dev.vendor_id, dev.device_id
    );

    // BAR0 (NAM) と BAR1 (NABM) を読み取る。
    // AC97 コントローラは I/O ポートマップドなので、BAR の bit0 が 1。
    // I/O ベースアドレスは bit[31:2] に格納されている。
    let bar0 = pci::read_bar(dev.bus, dev.device, dev.function, 0);
    let bar1 = pci::read_bar(dev.bus, dev.device, dev.function, 1);
    let nam_base = (bar0 & 0xFFFC) as u16;  // I/O ポートアドレス（bit0 のフラグを除去）
    let nabm_base = (bar1 & 0xFFFC) as u16;

    serial_println!("AC97: NAM base={:#06x}, NABM base={:#06x}", nam_base, nabm_base);

    // PCI Command Register (offset 0x04) に I/O Enable (bit 0) + Bus Master (bit 2) を設定。
    // これによりデバイスが I/O ポートアクセスと DMA を行えるようになる。
    let cmd = pci::pci_config_read16(dev.bus, dev.device, dev.function, 0x04);
    let new_cmd = cmd | 0x05; // bit 0 (I/O Space) | bit 2 (Bus Master)
    pci::pci_config_write16(dev.bus, dev.device, dev.function, 0x04, new_cmd);
    serial_println!("AC97: PCI command register: {:#06x} -> {:#06x}", cmd, new_cmd);

    // Global Control レジスタでコールドリセットを実行する。
    //
    // AC97 のリセット手順:
    //   1. Global Control = 0x00 でコールドリセットに入る（リセット信号をアサート）
    //   2. 一定時間待つ（コーデックがリセット状態を認識するまで）
    //   3. Global Control = 0x02 (Cold Reset Release) でリセットから復帰させる
    //   4. Global Status の Primary Codec Ready (bit 0) が立つのを待つ
    //
    // QEMU の AC97 エミュレーションでもこの手順が必要。

    // ステップ 1: コールドリセットに入る
    unsafe {
        Port::<u32>::new(nabm_base + GLOB_CNT).write(0x00);
    }

    // ステップ 2: リセット期間を待つ（100μs 程度必要、余裕を持って待つ）
    for _ in 0..100000 {
        core::hint::spin_loop();
    }

    // ステップ 3: コールドリセットから復帰
    // bit 1 = Cold Reset Release: コーデックへのリセット信号を解除する
    unsafe {
        Port::<u32>::new(nabm_base + GLOB_CNT).write(0x02);
    }

    // ステップ 4: Primary Codec Ready をポーリング
    // Global Status (GLOB_STA) の bit 8 = Primary Codec Ready
    // ※ QEMU の ICH AC97 エミュレーションでは bit 8 が Primary Ready
    //   （AC97 仕様では bit 0 だが、ICH では bit 8）
    let mut ready = false;
    for _ in 0..10000 {
        let status = unsafe { Port::<u32>::new(nabm_base + GLOB_STA).read() };
        // bit 8 (Primary Codec Ready) または bit 0 をチェック
        if status & 0x0100 != 0 {
            ready = true;
            serial_println!("AC97: global status = {:#010x}", status);
            break;
        }
        // 短いビジーウェイト（ポーリング間隔）
        for _ in 0..10000 {
            core::hint::spin_loop();
        }
    }

    if !ready {
        // デバッグ情報を出力
        let status = unsafe { Port::<u32>::new(nabm_base + GLOB_STA).read() };
        serial_println!("AC97: codec not ready (timeout), GLOB_STA={:#010x}", status);
        // QEMU ではコーデックが即座に ready になることもあるので、
        // ステータスに関わらず初期化を続行してみる
        serial_println!("AC97: continuing initialization anyway...");
    } else {
        serial_println!("AC97: codec ready");
    }

    // Mixer リセット: NAM の Reset レジスタに書き込む（値は無視される）。
    // これによりコーデックの全ミキサー設定がデフォルトに戻る。
    unsafe {
        Port::<u16>::new(nam_base + NAM_RESET).write(0x0000);
    }

    // 短い待機（コーデックのリセット完了を待つ）
    for _ in 0..100000 {
        core::hint::spin_loop();
    }

    // マスターボリュームを最大に設定。
    // 0x0000 = 左右とも 0dB（最大音量）、ミュートビット (bit 15) = 0。
    unsafe {
        Port::<u16>::new(nam_base + NAM_MASTER_VOL).write(0x0000);
    }

    // PCM 出力ボリュームを最大に設定。
    unsafe {
        Port::<u16>::new(nam_base + NAM_PCM_OUT_VOL).write(0x0000);
    }

    serial_println!("AC97: mixer initialized (master=0, pcm=0)");

    // BDL 用メモリを確保（32 エントリ × 8 bytes = 256 bytes、アライメント 4096）。
    // DMA ではデバイスが物理アドレスでメモリにアクセスするため、
    // UEFI 環境のアイデンティティマッピング（仮想アドレス == 物理アドレス）を前提とする。
    let bdl_layout = Layout::from_size_align(BDL_ENTRIES * 8, 4096)
        .expect("AC97: invalid BDL layout");
    let bdl_ptr = unsafe { alloc::alloc::alloc_zeroed(bdl_layout) };
    if bdl_ptr.is_null() {
        serial_println!("AC97: failed to allocate BDL memory");
        return;
    }

    // PCM バッファ用メモリを確保（32 バッファ × 16KB = 512KB、アライメント 4096）。
    let pcm_layout = Layout::from_size_align(BDL_ENTRIES * BYTES_PER_BUF, 4096)
        .expect("AC97: invalid PCM buffer layout");
    let pcm_buf_ptr = unsafe { alloc::alloc::alloc_zeroed(pcm_layout) };
    if pcm_buf_ptr.is_null() {
        serial_println!("AC97: failed to allocate PCM buffer memory");
        // BDL のメモリを解放
        unsafe { alloc::alloc::dealloc(bdl_ptr, bdl_layout); }
        return;
    }

    serial_println!(
        "AC97: BDL at {:#x}, PCM buffers at {:#x}",
        bdl_ptr as u64, pcm_buf_ptr as u64
    );

    let ac97 = Ac97 {
        nam_base,
        nabm_base,
        bdl_ptr: bdl_ptr as *mut BdlEntry,
        pcm_buf_ptr,
    };

    *AC97.lock() = Some(ac97);
    serial_println!("AC97: driver initialized");
}

impl Ac97 {
    /// 指定した周波数・持続時間で正弦波ビープ音を再生する。
    ///
    /// # 引数
    /// - `freq_hz`: 周波数 (Hz)。1〜20000 の範囲。
    /// - `duration_ms`: 持続時間 (ミリ秒)。1〜10000 の範囲。
    ///
    /// # 動作
    /// 1. sin ルックアップテーブルで正弦波の PCM データ（48kHz 16bit stereo）を生成
    /// 2. BDL にバッファ情報を設定
    /// 3. DMA エンジンを起動して再生
    /// 4. ステータスレジスタのポーリングで再生完了を待つ
    pub fn play_tone(&mut self, freq_hz: u32, duration_ms: u32) {
        // 総サンプル数を計算（48kHz × 持続時間）
        let total_samples = (SAMPLE_RATE as u64 * duration_ms as u64 / 1000) as usize;
        if total_samples == 0 {
            return;
        }

        // 使用するバッファ数を計算（切り上げ、最大 BDL_ENTRIES）
        let num_bufs = ((total_samples + SAMPLES_PER_BUF - 1) / SAMPLES_PER_BUF).min(BDL_ENTRIES);
        let mut remaining = total_samples;

        // 位相アキュムレータ（固定小数点、上位 8bit でテーブルインデックス）
        // phase_step = freq_hz * 65536 / SAMPLE_RATE
        // これにより 1 サンプルごとに位相が phase_step だけ進む。
        let phase_step = (freq_hz as u64 * 65536 / SAMPLE_RATE as u64) as u32;
        let mut phase: u32 = 0;

        for i in 0..num_bufs {
            let samples_in_buf = remaining.min(SAMPLES_PER_BUF);
            let buf_offset = i * BYTES_PER_BUF;
            let buf_addr = unsafe { self.pcm_buf_ptr.add(buf_offset) };

            // PCM データを生成（16bit stereo = 4 bytes/sample）
            // 左右チャンネルに同じ値を書き込む（モノラル的なステレオ）
            for s in 0..samples_in_buf {
                // sin テーブルのインデックス（上位 8bit）
                let idx = ((phase >> 8) & 0xFF) as usize;
                let value = SIN_TABLE[idx];

                // ステレオ: 左チャンネル (2 bytes) + 右チャンネル (2 bytes)
                let sample_offset = s * 4;
                let le_bytes = value.to_le_bytes();
                unsafe {
                    // 左チャンネル
                    *buf_addr.add(sample_offset) = le_bytes[0];
                    *buf_addr.add(sample_offset + 1) = le_bytes[1];
                    // 右チャンネル（同じ値）
                    *buf_addr.add(sample_offset + 2) = le_bytes[0];
                    *buf_addr.add(sample_offset + 3) = le_bytes[1];
                }

                // 位相を進める（16bit で自動的にラップアラウンド）
                phase = phase.wrapping_add(phase_step);
            }

            // BDL エントリを設定
            let bdl_entry = BdlEntry {
                addr: buf_addr as u32,  // 物理アドレス（アイデンティティマッピング前提）
                samples: samples_in_buf as u16,
                // 最後のバッファに IOC (Interrupt on Completion) フラグを立てる
                flags: if i == num_bufs - 1 { 0x8000 } else { 0 },
            };
            unsafe {
                *self.bdl_ptr.add(i) = bdl_entry;
            }

            remaining -= samples_in_buf;
        }

        // --- DMA エンジンの制御 ---

        // 1. PCM Out の Control Register を Reset (bit 1) してから停止状態にする
        unsafe {
            Port::<u8>::new(self.nabm_base + PO_CR).write(0x02); // Reset
        }
        // リセット完了を待つ
        for _ in 0..10000 {
            core::hint::spin_loop();
        }
        unsafe {
            Port::<u8>::new(self.nabm_base + PO_CR).write(0x00); // Clear reset
        }

        // 2. Status Register をクリア（write-clear: 全ビット 1 を書いてフラグをクリア）
        unsafe {
            Port::<u16>::new(self.nabm_base + PO_SR).write(0x1C); // Clear all status bits
        }

        // 3. BDL のベースアドレスを設定
        unsafe {
            Port::<u32>::new(self.nabm_base + PO_BDBAR).write(self.bdl_ptr as u32);
        }

        // 4. Last Valid Index を設定（使用するバッファの最後のインデックス）
        unsafe {
            Port::<u8>::new(self.nabm_base + PO_LVI).write((num_bufs - 1) as u8);
        }

        // 5. Run ビットを立てて DMA 転送を開始
        unsafe {
            Port::<u8>::new(self.nabm_base + PO_CR).write(0x01); // Run
        }

        serial_println!(
            "AC97: playing {}Hz for {}ms ({} samples, {} buffers)",
            freq_hz, duration_ms, total_samples, num_bufs
        );

        // 6. 再生完了をポーリングで待つ
        // CIV (Current Index Value) が LVI に追いつくか、
        // SR の BCH (Buffer Completion Half) / LVBCI (Last Valid Buffer Completion Interrupt)
        // ビットが立つのを待つ。
        // タイムアウトは duration_ms の 2 倍 + 100ms の余裕。
        let timeout_loops = (duration_ms as u64 + 100) * 10000;
        for _ in 0..timeout_loops {
            let sr = unsafe { Port::<u16>::new(self.nabm_base + PO_SR).read() };
            // bit 2 = LVBCI (Last Valid Buffer Completion Interrupt)
            // bit 3 = BCIS (Buffer Completion Interrupt Status)
            if sr & 0x04 != 0 {
                // LVBCI: 最後のバッファの再生が完了
                break;
            }
            core::hint::spin_loop();
        }

        // 7. DMA を停止
        unsafe {
            Port::<u8>::new(self.nabm_base + PO_CR).write(0x00); // Stop
        }

        // ステータスをクリア
        unsafe {
            Port::<u16>::new(self.nabm_base + PO_SR).write(0x1C);
        }

        serial_println!("AC97: playback finished");
    }

}

/// AC97 デバイスが利用可能かどうかを返す（selftest 用の便利関数）
pub fn is_available() -> bool {
    AC97.lock().is_some()
}
