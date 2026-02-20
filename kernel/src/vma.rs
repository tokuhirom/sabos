// vma.rs — Virtual Memory Area（仮想メモリ領域）管理
//
// プロセスのアドレス空間を VMA のリストとして管理する。
// 各 VMA は連続した仮想アドレス範囲を表し、用途（ELF ロード、スタック、匿名マッピング等）と
// アクセス権限（読み取り/書き込み/実行）を持つ。
//
// VMA リストを使うことで:
// - 空き仮想アドレス領域の検索が VMA 数に対する O(n) で済む（ページテーブル走査不要）
// - プロセスのメモリレイアウトを一覧できる（/proc/maps）
// - munmap で部分的なアンマップ（VMA の分割）ができる
// - 将来の Demand Paging 実装の基盤になる

use alloc::string::String;
use alloc::vec::Vec;

/// VMA の種類（何のためにマッピングされた領域か）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmaKind {
    /// 匿名マッピング（mmap で確保されたヒープ等）
    Anonymous,
    /// ELF の LOAD セグメント（.text, .rodata, .data, .bss）
    ElfLoad,
    /// ユーザースタック
    UserStack,
}

/// VMA のアクセス権限
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmaProt {
    /// 読み取り可能か
    pub read: bool,
    /// 書き込み可能か
    pub write: bool,
    /// 実行可能か
    pub execute: bool,
}

impl VmaProt {
    /// ELF セグメントフラグ (PF_R=4, PF_W=2, PF_X=1) から VmaProt を作成する
    pub fn from_elf_flags(flags: u32) -> Self {
        Self {
            read: (flags & 4) != 0,
            write: (flags & 2) != 0,
            execute: (flags & 1) != 0,
        }
    }

    /// 読み書き可能、実行不可の権限を返す（スタック、匿名マッピング用）
    pub fn read_write() -> Self {
        Self {
            read: true,
            write: true,
            execute: false,
        }
    }

    /// 読み取り専用の権限を返す
    pub fn read_only() -> Self {
        Self {
            read: true,
            write: false,
            execute: false,
        }
    }
}

/// 仮想メモリ領域（Virtual Memory Area）
///
/// プロセスのアドレス空間における 1 つの連続領域を表す。
/// start と end は 4KiB アラインされている必要がある。
/// end は排他的（start <= addr < end がこの VMA に属する）。
#[derive(Debug, Clone)]
pub struct Vma {
    /// 開始仮想アドレス（4KiB アライン、包含的）
    pub start: u64,
    /// 終了仮想アドレス（4KiB アライン、排他的）
    pub end: u64,
    /// アクセス権限
    pub prot: VmaProt,
    /// VMA の種類
    pub kind: VmaKind,
    /// デバッグ用の名前（"[anon]", "[stack]", ".text" 等）
    pub name: String,
}

impl Vma {
    /// VMA のサイズ（バイト数）を返す
    pub fn size(&self) -> u64 {
        self.end - self.start
    }

    /// 他の VMA と重なるか（隣接は OK、重なりは NG）
    pub fn overlaps(&self, other: &Vma) -> bool {
        self.start < other.end && other.start < self.end
    }
}

/// VMA のリスト（プロセスごとに 1 つ持つ）
///
/// 内部は start 昇順でソートされた Vec<Vma>。
/// VMA 数は通常数十程度なので、Vec + 線形/二分探索で十分な性能が出る。
pub struct VmaList {
    /// start 昇順ソートされた VMA のリスト
    vmas: Vec<Vma>,
}

impl VmaList {
    /// 空の VmaList を作成する
    pub fn new() -> Self {
        Self {
            vmas: Vec::new(),
        }
    }

    /// VMA の数を返す
    pub fn len(&self) -> usize {
        self.vmas.len()
    }

    /// VMA リストのイテレータを返す
    pub fn iter(&self) -> core::slice::Iter<'_, Vma> {
        self.vmas.iter()
    }

    /// VMA を挿入する。
    ///
    /// 既存の VMA と重なる場合は Err を返す。
    /// 重複がなければソート順を維持して挿入し Ok を返す。
    pub fn insert(&mut self, vma: Vma) -> Result<(), &'static str> {
        // 重複チェック: 既存 VMA のいずれかと重なっていないか確認
        for existing in &self.vmas {
            if vma.overlaps(existing) {
                return Err("VMA overlaps with existing region");
            }
        }

        // 挿入位置を二分探索で決定（start 昇順）
        let pos = self.vmas.partition_point(|v| v.start < vma.start);
        self.vmas.insert(pos, vma);

        Ok(())
    }

    /// 指定した範囲 [base, limit) の中で、size バイト以上の空き領域を探す（first-fit）。
    ///
    /// VMA リストの隙間を走査して、最初に見つかった十分な空き領域の開始アドレスを返す。
    /// ページテーブルを走査する旧実装と異なり、VMA 数に対する O(n) で済む。
    ///
    /// # 引数
    /// - `size`: 必要なバイト数（4KiB アラインされている前提）
    /// - `base`: 探索範囲の下限アドレス
    /// - `limit`: 探索範囲の上限アドレス
    ///
    /// # 戻り値
    /// 空き領域の開始アドレス。見つからなければ None。
    pub fn find_free_region(&self, size: u64, base: u64, limit: u64) -> Option<u64> {
        // 候補の開始点
        let mut candidate = base;

        for vma in &self.vmas {
            // この VMA が探索範囲外なら無視
            if vma.end <= base {
                continue;
            }
            if vma.start >= limit {
                break;
            }

            // candidate から vma.start までの隙間が十分か
            if vma.start > candidate && vma.start - candidate >= size {
                return Some(candidate);
            }

            // この VMA の後ろから再スタート
            if vma.end > candidate {
                candidate = vma.end;
            }
        }

        // 最後の VMA の後〜limit までの隙間をチェック
        if candidate + size <= limit {
            Some(candidate)
        } else {
            None
        }
    }

    /// 指定した範囲 [start, end) に重なる VMA を削除・分割する。
    ///
    /// 部分的に重なる VMA は分割される:
    /// - VMA 全体が範囲内 → 完全削除
    /// - VMA の先頭が範囲内 → 先頭を切り取り（VMA の start を end に変更）
    /// - VMA の末尾が範囲内 → 末尾を切り取り（VMA の end を start に変更）
    /// - VMA の中央が範囲 → VMA を 2 つに分割
    ///
    /// 返り値は削除・変更された VMA のリスト（デバッグ用）。
    pub fn remove_range(&mut self, start: u64, end: u64) -> Vec<Vma> {
        let mut removed = Vec::new();
        let mut new_vmas = Vec::new();

        for vma in self.vmas.drain(..) {
            if vma.end <= start || vma.start >= end {
                // この VMA は範囲外 → そのまま残す
                new_vmas.push(vma);
            } else if vma.start >= start && vma.end <= end {
                // VMA 全体が範囲内 → 完全削除
                removed.push(vma);
            } else if vma.start < start && vma.end > end {
                // VMA の中央が範囲 → 2 つに分割
                // 前半部分
                new_vmas.push(Vma {
                    start: vma.start,
                    end: start,
                    prot: vma.prot.clone(),
                    kind: vma.kind.clone(),
                    name: vma.name.clone(),
                });
                // 後半部分
                new_vmas.push(Vma {
                    start: end,
                    end: vma.end,
                    prot: vma.prot.clone(),
                    kind: vma.kind.clone(),
                    name: vma.name.clone(),
                });
                removed.push(vma);
            } else if vma.start < start {
                // VMA の末尾が範囲内 → 末尾を切り取り
                let original = vma.clone();
                new_vmas.push(Vma {
                    start: vma.start,
                    end: start,
                    prot: vma.prot,
                    kind: vma.kind,
                    name: vma.name,
                });
                removed.push(original);
            } else {
                // vma.end > end: VMA の先頭が範囲内 → 先頭を切り取り
                let original = vma.clone();
                new_vmas.push(Vma {
                    start: end,
                    end: vma.end,
                    prot: vma.prot,
                    kind: vma.kind,
                    name: vma.name,
                });
                removed.push(original);
            }
        }

        self.vmas = new_vmas;
        removed
    }

}
