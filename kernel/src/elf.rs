// elf.rs — ELF64 バイナリパーサー
//
// ELF (Executable and Linkable Format) は Unix 系 OS で標準的な実行ファイル形式。
// このモジュールでは ELF64 バイナリの最小限のパースを行い、
// カーネルが Ring 3 で実行するために必要な情報を抽出する。
//
// ELF ファイルの構造:
//   1. ELF ヘッダー (64 バイト) — マジック、アーキテクチャ、エントリポイント等
//   2. プログラムヘッダーテーブル — メモリにロードするセグメントの情報
//   3. セクションヘッダーテーブル — リンカ/デバッガ向けのセクション情報（今回は不使用）
//
// カーネルが必要とするのは:
//   - エントリポイントのアドレス (e_entry)
//   - PT_LOAD セグメント: メモリにロードすべきデータの位置・サイズ・仮想アドレス
//
// PT_LOAD セグメントは「ファイルのこの部分を、仮想アドレスのここにロードしろ」という指示。
// p_filesz（ファイル上のサイズ）と p_memsz（メモリ上のサイズ）が異なる場合、
// 差分は BSS（未初期化データ）としてゼロで埋める。

use alloc::vec::Vec;

// =================================================================
// ELF64 ヘッダー構造体
// =================================================================

/// ELF64 ファイルヘッダー。
///
/// ELF ファイルの先頭 64 バイトに配置される。
/// マジックナンバー、アーキテクチャ、エントリポイントアドレス等の
/// 基本情報を格納する。
///
/// #[repr(C)] は C 言語と同じメモリレイアウトを保証する。
/// Rust のデフォルトレイアウトはフィールドの並び替えが許可されるが、
/// ELF 仕様の固定レイアウトに合わせるために #[repr(C)] が必須。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Elf64Header {
    /// ELF マジック + 識別情報 (16 バイト)
    /// e_ident[0..4] = [0x7f, 'E', 'L', 'F'] — ELF マジックナンバー
    /// e_ident[4] = 2 — 64ビット (ELFCLASS64)
    /// e_ident[5] = 1 — リトルエンディアン (ELFDATA2LSB)
    pub e_ident: [u8; 16],
    /// ファイルタイプ (2 = ET_EXEC: 実行可能ファイル)
    pub e_type: u16,
    /// ターゲットアーキテクチャ (0x3E = EM_X86_64)
    pub e_machine: u16,
    /// ELF バージョン (1 = EV_CURRENT)
    pub e_version: u32,
    /// エントリポイントの仮想アドレス（プログラム開始位置）
    pub e_entry: u64,
    /// プログラムヘッダーテーブルのファイル内オフセット
    pub e_phoff: u64,
    /// セクションヘッダーテーブルのファイル内オフセット（今回は不使用）
    pub e_shoff: u64,
    /// プロセッサ固有フラグ
    pub e_flags: u32,
    /// このヘッダーのサイズ (64 バイト)
    pub e_ehsize: u16,
    /// プログラムヘッダー1エントリのサイズ (56 バイト)
    pub e_phentsize: u16,
    /// プログラムヘッダーのエントリ数
    pub e_phnum: u16,
    /// セクションヘッダー1エントリのサイズ
    pub e_shentsize: u16,
    /// セクションヘッダーのエントリ数
    pub e_shnum: u16,
    /// セクション名文字列テーブルのインデックス
    pub e_shstrndx: u16,
}

// =================================================================
// ELF64 プログラムヘッダー構造体
// =================================================================

/// ELF64 プログラムヘッダー。
///
/// 各プログラムヘッダーは 1 つの「セグメント」を記述する。
/// PT_LOAD タイプのセグメントは、ELF ファイルの一部をメモリの
/// 指定アドレスにロードすることを指示する。
///
/// 主要フィールド:
///   p_offset — ファイル内のデータ開始位置
///   p_vaddr  — ロード先の仮想アドレス
///   p_filesz — ファイル上のデータサイズ（これだけコピーする）
///   p_memsz  — メモリ上のサイズ（p_filesz より大きい場合、残りはゼロ = BSS）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Elf64ProgramHeader {
    /// セグメントタイプ (1 = PT_LOAD)
    pub p_type: u32,
    /// セグメントフラグ (PF_X=1, PF_W=2, PF_R=4)
    pub p_flags: u32,
    /// ファイル内のデータ開始オフセット
    pub p_offset: u64,
    /// ロード先の仮想アドレス
    pub p_vaddr: u64,
    /// ロード先の物理アドレス（OS がページングを使う場合は通常 p_vaddr と同じ）
    pub p_paddr: u64,
    /// ファイル内のデータサイズ（バイト）
    pub p_filesz: u64,
    /// メモリ上のサイズ（バイト）。p_filesz より大きい場合、差分は BSS
    pub p_memsz: u64,
    /// セグメントのアライメント要求
    pub p_align: u64,
}

/// PT_LOAD セグメントタイプ。メモリにロードするセグメントを示す。
const PT_LOAD: u32 = 1;

/// EM_X86_64: x86_64 アーキテクチャを示す e_machine の値。
const EM_X86_64: u16 = 0x3E;

// =================================================================
// パース結果
// =================================================================

/// PT_LOAD セグメントの情報。
/// ELF ローダーがメモリにデータをコピーするために必要な情報をまとめる。
#[derive(Debug, Clone)]
pub struct LoadSegment {
    /// ロード先の仮想アドレス
    pub vaddr: u64,
    /// メモリ上のサイズ（BSS 含む）
    pub memsz: u64,
    /// ファイル内のデータ開始オフセット
    pub offset: u64,
    /// ファイル内のデータサイズ
    pub filesz: u64,
    /// セグメントフラグ (PF_X=1, PF_W=2, PF_R=4)
    pub flags: u32,
}

/// ELF パース結果。
/// カーネルが ELF バイナリをメモリにロードして実行するために必要な情報。
#[derive(Debug)]
pub struct ElfInfo {
    /// エントリポイントの仮想アドレス（プログラム開始位置）
    pub entry_point: u64,
    /// PT_LOAD セグメントのリスト
    pub load_segments: Vec<LoadSegment>,
}

// =================================================================
// パース関数
// =================================================================

/// ELF64 バイナリをパースして ElfInfo を返す。
///
/// 検証項目:
///   1. ELF マジックナンバー (0x7f, 'E', 'L', 'F')
///   2. 64ビットクラス (ELFCLASS64)
///   3. リトルエンディアン (ELFDATA2LSB)
///   4. ターゲットアーキテクチャ (EM_X86_64)
///   5. プログラムヘッダーのサイズが正しいこと
///
/// PT_LOAD セグメントを抽出してリストにする。
/// PT_LOAD 以外のセグメント (GNU_STACK, GNU_EH_FRAME 等) は無視する。
pub fn parse_elf(data: &[u8]) -> Result<ElfInfo, &'static str> {
    // --- ELF ヘッダーの読み取り ---
    let header_size = core::mem::size_of::<Elf64Header>();
    if data.len() < header_size {
        return Err("データが ELF ヘッダーサイズより小さい");
    }

    // バイト列を Elf64Header 構造体として解釈する。
    // #[repr(C)] なのでメモリレイアウトが C と同じ = ELF 仕様通り。
    // Safety: data のサイズは header_size 以上であることを確認済み。
    let header: &Elf64Header = unsafe {
        &*(data.as_ptr() as *const Elf64Header)
    };

    // --- マジックナンバーの検証 ---
    // ELF ファイルは必ず 0x7f 'E' 'L' 'F' で始まる
    if &header.e_ident[0..4] != b"\x7fELF" {
        return Err("ELF マジックナンバーが不正");
    }

    // --- 64ビットクラスの検証 ---
    // e_ident[4] = 2 は ELFCLASS64（64ビット）
    if header.e_ident[4] != 2 {
        return Err("ELF64 ではない（ELFCLASS64 が必要）");
    }

    // --- エンディアンの検証 ---
    // e_ident[5] = 1 は ELFDATA2LSB（リトルエンディアン）
    // x86_64 はリトルエンディアンなのでこれが必須
    if header.e_ident[5] != 1 {
        return Err("リトルエンディアンではない");
    }

    // --- ターゲットアーキテクチャの検証 ---
    if header.e_machine != EM_X86_64 {
        return Err("x86_64 アーキテクチャではない");
    }

    // --- プログラムヘッダーのサイズ検証 ---
    let ph_entry_size = core::mem::size_of::<Elf64ProgramHeader>();
    if header.e_phentsize as usize != ph_entry_size {
        return Err("プログラムヘッダーのエントリサイズが不正");
    }

    // --- プログラムヘッダーテーブルの範囲チェック ---
    let ph_offset = header.e_phoff as usize;
    let ph_count = header.e_phnum as usize;
    let ph_table_size = ph_count * ph_entry_size;
    if data.len() < ph_offset + ph_table_size {
        return Err("プログラムヘッダーテーブルがデータ範囲外");
    }

    // --- PT_LOAD セグメントの抽出 ---
    let mut load_segments = Vec::new();

    for i in 0..ph_count {
        let ph_start = ph_offset + i * ph_entry_size;
        // Safety: 範囲チェック済み
        let ph: &Elf64ProgramHeader = unsafe {
            &*(data[ph_start..].as_ptr() as *const Elf64ProgramHeader)
        };

        // PT_LOAD 以外のセグメント (GNU_STACK, GNU_EH_FRAME 等) は無視
        if ph.p_type != PT_LOAD {
            continue;
        }

        // セグメントデータがファイル範囲内にあることを確認
        let seg_end = ph.p_offset as usize + ph.p_filesz as usize;
        if seg_end > data.len() {
            return Err("LOAD セグメントのデータがファイル範囲外");
        }

        load_segments.push(LoadSegment {
            vaddr: ph.p_vaddr,
            memsz: ph.p_memsz,
            offset: ph.p_offset,
            filesz: ph.p_filesz,
            flags: ph.p_flags,
        });
    }

    if load_segments.is_empty() {
        return Err("PT_LOAD セグメントが見つからない");
    }

    Ok(ElfInfo {
        entry_point: header.e_entry,
        load_segments,
    })
}
