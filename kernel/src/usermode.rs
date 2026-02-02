// usermode.rs — Ring 3（ユーザーモード）への遷移とユーザープログラム
//
// x86_64 CPU には Ring 0〜3 の4つの特権レベル（リング）がある。
// Ring 0 が最高特権（カーネル）、Ring 3 が最低特権（ユーザー）。
// 通常の OS ではカーネルが Ring 0 で動き、アプリケーションは Ring 3 で動く。
//
// Ring 3 のコードは特権命令（I/O ポートアクセス、CR レジスタ操作等）を
// 直接実行できない。カーネルの機能が必要な場合は「システムコール」で
// Ring 0 に制御を移す必要がある。
//
// Ring 0 → Ring 3 への遷移は iretq 命令を使う。
// iretq はスタック上の RIP/CS/RFLAGS/RSP/SS を pop して CPU 状態を復帰する。
// CS と SS に Ring 3 のセグメントセレクタ（RPL=3）をセットしておくことで、
// Ring 3 に「戻る」（実際は初めて遷移する）ことができる。
//
// Ring 3 → Ring 0 への遷移は int 0x80（ソフトウェア割り込み）を使う。
// このとき CPU は TSS の rsp0 からカーネルスタックのアドレスを読み取り、
// 自動的にスタックを切り替える。

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::arch::{asm, global_asm};
use x86_64::structures::paging::{PhysFrame, Size4KiB};
use x86_64::VirtAddr;

use crate::gdt;

/// ユーザープログラムの ELF バイナリ。
/// user/ crate をビルドした結果の ELF64 バイナリを、include_bytes! でカーネルに埋め込む。
/// UEFI Boot Services 終了後はファイルシステムにアクセスできないため、
/// コンパイル時にカーネルバイナリに含めておく方式をとる。
///
/// ビルド順序: make build-user → make build (cargo build が include_bytes! を処理)
static USER_ELF_DATA: &[u8] = include_bytes!("../../user/target/x86_64-unknown-none/debug/sabos-user");

/// ユーザーモード用のスタック（16KiB）。
/// Ring 3 で実行されるコードが使うスタック領域。
/// カーネルスタックとは別に用意する必要がある。
/// 現在は全プロセスで共有の static 領域を使う（将来的にはプロセスごとに動的確保）。
const USER_STACK_SIZE: usize = 4096 * 4; // 16KiB
static mut USER_STACK: [u8; USER_STACK_SIZE] = [0; USER_STACK_SIZE];

/// カーネルスタックのサイズ（16KiB）。
/// Ring 3 → Ring 0 遷移時に CPU が TSS rsp0 経由で切り替えるスタック。
const KERNEL_STACK_SIZE: usize = 4096 * 4; // 16KiB

// =================================================================
// UserProcess — プロセスごとのアドレス空間を管理
// =================================================================

/// ユーザープロセスを表す構造体。
///
/// プロセスごとに専用の L4 ページテーブルを持ち、CR3 を切り替えることで
/// アドレス空間を分離する。カーネル空間のマッピングは全プロセスで共有される。
pub struct UserProcess {
    /// プロセス固有の L4 ページテーブルが配置された物理フレーム。
    /// CR3 にこのフレームのアドレスを書き込むとアドレス空間が切り替わる。
    pub page_table_frame: PhysFrame<Size4KiB>,
    /// カーネルスタック（Ring 3 → Ring 0 遷移時に TSS rsp0 経由で使う）。
    /// Box で確保することでプロセスごとに独立したカーネルスタックを持てる。
    pub kernel_stack: Box<[u8]>,
    /// ELF ローダーが確保した物理フレームのリスト。
    /// プロセス破棄時にこれらのフレームをフレームアロケータに返却する。
    /// 既存の create_user_process() では空 Vec（カーネル既存マッピングを流用するため）。
    /// create_elf_process() では LOAD セグメント + ユーザースタック用のフレームが入る。
    pub allocated_frames: Vec<PhysFrame<Size4KiB>>,
}

/// ユーザープロセスを作成する。
///
/// 1. create_process_page_table() で L4 ページテーブルを作成
/// 2. set_user_accessible_in_process() でユーザースタック・コード・データ領域を設定
/// 3. カーネルスタックを Box で確保
///
/// 返り値の UserProcess を run_in_usermode() に渡してプログラムを実行する。
pub fn create_user_process(program: &UserProgram) -> UserProcess {
    // 1. プロセス固有のページテーブルを作成
    let page_table_frame = crate::paging::create_process_page_table();

    // 2. ユーザースタックを USER_ACCESSIBLE に設定
    let user_stack_addr = &raw const USER_STACK as u64;
    crate::paging::set_user_accessible_in_process(
        page_table_frame,
        VirtAddr::new(user_stack_addr),
        USER_STACK_SIZE,
    );

    // 3. ユーザーコードのページを USER_ACCESSIBLE に設定
    //    エントリポイントの関数を含むページ。関数サイズは正確にはわからないので、
    //    余裕をもって 2 ページ (8KiB) 分を設定する。
    let entry_addr = program.entry as *const () as u64;
    let code_size = 8192; // 2 ページ分
    crate::paging::set_user_accessible_in_process(
        page_table_frame,
        VirtAddr::new(entry_addr),
        code_size,
    );

    // 4. データ領域（文字列リテラル等）を USER_ACCESSIBLE に設定
    for &(data_addr, data_size) in program.data_regions {
        if data_size > 0 {
            crate::paging::set_user_accessible_in_process(
                page_table_frame,
                VirtAddr::new(data_addr),
                data_size,
            );
        }
    }

    // 5. カーネルスタックを確保（プロセスごとに独立）
    let kernel_stack = alloc::vec![0u8; KERNEL_STACK_SIZE].into_boxed_slice();

    UserProcess {
        page_table_frame,
        kernel_stack,
        allocated_frames: Vec::new(),
    }
}

/// ユーザープロセスを破棄する。
///
/// 1. ELF ローダーが確保した物理フレームをフレームアロケータに返却
/// 2. プロセス固有のページテーブル（分岐コピーしたテーブル含む）を解放
/// 3. kernel_stack は Box の Drop で自動解放
pub fn destroy_user_process(process: UserProcess) {
    // ELF ローダーが確保したデータ用フレームを解放する。
    // これらのフレームは LOAD セグメントのデータやユーザースタック用に確保されたもの。
    // ページテーブルの中間テーブル用フレームは destroy_process_page_table() が解放する。
    if !process.allocated_frames.is_empty() {
        let mut fa = crate::memory::FRAME_ALLOCATOR.lock();
        for frame in &process.allocated_frames {
            unsafe {
                fa.deallocate_frame(*frame);
            }
        }
    }
    crate::paging::destroy_process_page_table(process.page_table_frame);
    // kernel_stack は process がドロップされるときに自動解放
}

/// run_in_usermode() のカーネルスタック状態を保存するグローバル変数。
/// SYS_EXIT システムコールで、ここに保存した RSP/RBP を復元して
/// run_in_usermode() の呼び出し元に return する（setjmp/longjmp パターン）。
static mut SAVED_RSP: u64 = 0;
static mut SAVED_RBP: u64 = 0;

// =================================================================
// iretq による Ring 3 への遷移（アセンブリ）
// =================================================================
//
// jump_to_usermode は Rust から呼ばれるアセンブリ関数。
// Microsoft x64 ABI に従って引数を受け取る:
//   rcx = entry_addr (RIP)
//   rdx = user_cs (CS)
//   r8  = rflags (RFLAGS)
//   r9  = user_stack_top (RSP)
//   スタック上 [rsp+40] = user_ss (SS)  ← 5番目の引数
//
// 手順:
//   1. 現在の RSP/RBP を SAVED_RSP/SAVED_RBP に保存
//   2. iretq 用スタックフレームを構築
//   3. iretq で Ring 3 に遷移
//
// exit_usermode() が SAVED_RSP/SAVED_RBP を復元して ret すると、
// この関数の呼び出し元（run_in_usermode 内）に戻る。
global_asm!(
    ".global jump_to_usermode",
    "jump_to_usermode:",
    // 現在の RSP/RBP を保存する（exit_usermode で戻るため）
    // この時点の RSP には call 命令が push したリターンアドレスがあり、
    // ret で呼び出し元に戻れる状態。
    "mov [rip + {saved_rsp}], rsp",
    "mov [rip + {saved_rbp}], rbp",

    // 5番目の引数 (user_ss) はスタック上にある。
    // Microsoft x64 ABI では、call 命令でリターンアドレスが push されるので、
    // [rsp+8] がシャドウスペース開始、[rsp+40] が5番目の引数。
    "mov rax, [rsp + 40]",

    // iretq 用スタックフレームを構築する。
    // iretq は以下の順で pop する:
    //   RIP → CS → RFLAGS → RSP → SS
    // なので push は逆順:
    "push rax",    // SS = user_ss
    "push r9",     // RSP = user_stack_top
    "push r8",     // RFLAGS (IF=1)
    "push rdx",    // CS = user_cs
    "push rcx",    // RIP = entry_addr
    "iretq",       // Ring 3 へ遷移！
    saved_rsp = sym SAVED_RSP,
    saved_rbp = sym SAVED_RBP,
);

unsafe extern "C" {
    /// アセンブリで定義した Ring 3 遷移関数。
    /// Microsoft x64 ABI: jump_to_usermode(entry_addr, user_cs, rflags, user_stack_top, user_ss)
    fn jump_to_usermode(
        entry_addr: u64,
        user_cs: u64,
        rflags: u64,
        user_stack_top: u64,
        user_ss: u64,
    );
}

/// ユーザーモードでプログラムを実行する（プロセスページテーブル + CR3 切り替え方式）。
///
/// 手順:
///   1. ユーザースタックのトップアドレスを計算
///   2. カーネルスタックのトップを TSS rsp0 に設定
///   3. CR3 をプロセスのページテーブルに切り替え
///   4. jump_to_usermode() で iretq による Ring 3 遷移
///   5. 戻り後、CR3 をカーネルのページテーブルに復帰
///
/// プロセスのページテーブルには create_user_process() で USER_ACCESSIBLE が
/// 設定済みなので、set/clear_user_accessible() は不要。
/// CR3 を切り替えるだけでアドレス空間が丸ごと切り替わる。
///
/// SYS_EXIT システムコールが呼ばれると、exit_usermode() 経由で
/// SAVED_RSP/SAVED_RBP を復元し、jump_to_usermode() の呼び出しから
/// 正常に return する。結果的にこの関数も正常に return する。
///
/// Ring 3 でページフォルトが発生した場合も exit_usermode() で戻る。
pub fn run_in_usermode(process: &UserProcess, program: &UserProgram) {
    // ユーザースタックのトップアドレスを計算。
    // スタックは高いアドレスから低いアドレスに向かって伸びるので、
    // 配列の末尾がスタックトップになる。
    let user_stack_addr = &raw const USER_STACK as u64;
    let user_stack_top = user_stack_addr + USER_STACK_SIZE as u64;

    // カーネルスタックのトップアドレスを計算。
    // Ring 3 で int 0x80 が発生したとき、CPU は TSS rsp0 のアドレスに
    // スタックを切り替える。このスタックでシステムコールハンドラが動く。
    let kernel_stack_top = {
        let start = process.kernel_stack.as_ptr() as u64;
        start + process.kernel_stack.len() as u64
    };

    // TSS rsp0 にカーネルスタックのトップを設定する。
    // これを忘れると Ring 3 → Ring 0 遷移時に rsp0=0 になり triple fault する。
    unsafe {
        gdt::set_tss_rsp0(VirtAddr::new(kernel_stack_top));
    }

    // セグメントセレクタを取得。
    // iretq で push する CS/SS の値。RPL=3 が含まれている。
    let user_cs = gdt::user_code_selector().0 as u64;
    let user_ss = gdt::user_data_selector().0 as u64;

    // RFLAGS: IF (Interrupt Flag, bit 9) を立てておく。
    // IF=0 だと Ring 3 でタイマー割り込みが無効のままになり、
    // プリエンプション（強制タスク切り替え）が機能しなくなる。
    let rflags: u64 = 0x200; // IF=1

    // エントリポイントのアドレス
    let entry_point = program.entry;
    let entry_addr = entry_point as *const () as u64;

    // --- CR3 をプロセスのページテーブルに切り替え ---
    // カーネルマッピングは共有されているので、カーネルコードは正常に動作する。
    // プロセスのページテーブルには USER_ACCESSIBLE が設定済み。
    unsafe {
        crate::paging::switch_to_process_page_table(process.page_table_frame);
    }

    // Ring 3 に遷移する。
    // jump_to_usermode() は内部で RSP/RBP を保存し、iretq で Ring 3 に飛ぶ。
    // SYS_EXIT → exit_usermode() で RSP/RBP が復元され、
    // jump_to_usermode() の呼び出しが正常に return したように見える。
    // ページフォルトの場合も exit_usermode() で戻ってくる。
    unsafe {
        jump_to_usermode(entry_addr, user_cs, rflags, user_stack_top, user_ss);
    }

    // ここに到達 = exit_usermode() 経由で Ring 3 から戻ってきた

    // --- CR3 をカーネルのページテーブルに復帰 ---
    // プロセスのページテーブルからカーネルのページテーブルに戻す。
    // これにより USER_ACCESSIBLE が設定されていないカーネルのページテーブルに戻る。
    unsafe {
        crate::paging::switch_to_kernel_page_table();
    }
}

/// ユーザーモードからカーネルに戻る（SYS_EXIT から呼ばれる）。
///
/// jump_to_usermode() で保存した RSP/RBP を復元し、ret で
/// jump_to_usermode() の呼び出し元（run_in_usermode 内）に戻る。
/// setjmp/longjmp パターンの longjmp に相当する。
///
/// # Safety
/// この関数は syscall_dispatch() の SYS_EXIT ハンドラからのみ呼ぶこと。
/// それ以外の場所から呼ぶとスタックが不整合になり未定義動作になる。
pub fn exit_usermode() -> ! {
    unsafe {
        asm!(
            // jump_to_usermode() が保存した RSP/RBP を復元する。
            // RSP にはリターンアドレスがあり、ret で呼び出し元に戻れる。
            "mov rsp, [{saved_rsp}]",
            "mov rbp, [{saved_rbp}]",
            "ret", // jump_to_usermode() の呼び出し元（run_in_usermode 内）に戻る
            saved_rsp = sym SAVED_RSP,
            saved_rbp = sym SAVED_RBP,
            options(noreturn),
        );
    }
}

// =================================================================
// ユーザープログラム
// =================================================================
//
// Ring 3 で実行される関数。カーネル関数（kprint! 等）は直接呼べない。
// 文字列を出力するには int 0x80 でシステムコール SYS_WRITE を呼ぶ。
// プログラムを終了するには SYS_EXIT を呼ぶ。

/// ユーザープログラムの情報を保持する構造体。
///
/// エントリポイント（関数ポインタ）に加え、Ring 3 からアクセスが必要な
/// データ領域（文字列リテラル等）のアドレスとサイズを保持する。
/// run_in_usermode() はこの情報をもとに USER_ACCESSIBLE を設定する。
pub struct UserProgram {
    /// ユーザープログラムのエントリポイント
    pub entry: fn(),
    /// Ring 3 からアクセスが必要なデータ領域の一覧 (アドレス, サイズ)
    /// 文字列リテラルなど .rodata に配置されるデータを含む
    pub data_regions: &'static [(u64, usize)],
}

/// Hello World プログラムで使う文字列リテラル。
/// static に置くことでアドレスが固定され、UserProgram から参照できる。
static USER_HELLO_MSG: &[u8] = b"Hello from Ring 3!\n";


/// Hello World ユーザープログラムの UserProgram を返す。
pub fn get_user_hello() -> UserProgram {
    UserProgram {
        entry: user_hello,
        data_regions: {
            // データ領域のアドレスを動的に計算する。
            // static 配列は変更できないので、代わりにスタック上で構築して
            // 'static ライフタイムにリークする（学習用OSなのでメモリリークは許容）。
            let regions = alloc::vec![
                (USER_HELLO_MSG.as_ptr() as u64, USER_HELLO_MSG.len()),
            ];
            // Vec を Box<[T]> に変換して leak で 'static 参照にする
            alloc::boxed::Box::leak(regions.into_boxed_slice())
        },
    }
}

/// Ring 3 で実行される Hello World プログラム。
///
/// この関数はカーネルバイナリの一部としてコンパイルされるが、
/// Ring 3 の特権レベルで実行される。
/// 文字列リテラルは USER_HELLO_MSG として static に配置され、
/// run_in_usermode() が USER_ACCESSIBLE を設定してからアクセスする。
pub fn user_hello() {
    let ptr = USER_HELLO_MSG.as_ptr() as u64;
    let len = USER_HELLO_MSG.len() as u64;

    unsafe {
        // SYS_WRITE (1): カーネルコンソールに文字列を出力する。
        // rax = 1 (SYS_WRITE)
        // rdi = バッファのポインタ
        // rsi = バッファの長さ
        asm!(
            "int 0x80",
            in("rax") 1u64,    // SYS_WRITE
            in("rdi") ptr,     // buf_ptr
            in("rsi") len,     // buf_len
            // int 0x80 は rax を戻り値で上書きする
            lateout("rax") _,
        );

        // SYS_EXIT (60): ユーザープログラムを終了する。
        // カーネル側で保存したスタックを復元して run_in_usermode() から return する。
        asm!(
            "int 0x80",
            in("rax") 60u64,   // SYS_EXIT
            options(noreturn),
        );
    }
}

// =================================================================
// テスト用: カーネルメモリへの不正アクセスプログラム
// =================================================================

/// カーネルメモリへの不正アクセスを試みるテストプログラムの UserProgram を返す。
///
/// このプログラムは Ring 3 からカーネル空間のメモリにアクセスしようとする。
/// USER_ACCESSIBLE が設定されていないアドレスへの読み込みなので、
/// ページフォルト (#PF) が発生し、graceful に終了するはず。
pub fn get_user_illegal_access() -> UserProgram {
    UserProgram {
        entry: user_illegal_access,
        // 不正アクセステストなのでデータ領域は不要
        data_regions: &[],
    }
}

/// カーネルメモリへの不正アクセスを試みるテストプログラム。
///
/// Ring 3 で実行され、USER_ACCESSIBLE が設定されていないアドレス (0x0) を
/// 読み込もうとする。これにより Page Fault (#PF) が発生し、
/// page_fault_handler が USER_MODE ビットを検出して exit_usermode() を呼ぶ。
/// 結果的に run_in_usermode() が正常に return し、シェルに安全に戻る。
pub fn user_illegal_access() {
    // アドレス 0x0 はカーネル空間（USER_ACCESSIBLE なし）
    // Ring 3 からここを読もうとすると Page Fault が発生する。
    // → page_fault_handler → exit_usermode() で安全にカーネルに戻る。
    unsafe {
        core::ptr::read_volatile(0x0 as *const u8);
    }
}
