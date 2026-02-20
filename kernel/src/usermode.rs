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
use alloc::string::String;
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
    ///
    /// VMA リストとは役割が異なる:
    /// - allocated_frames: 物理フレームの所有権管理（解放用）
    /// - vma_list: 仮想アドレス空間のマッピング意図管理（検索・分割用）
    /// 将来の Demand Paging では VMA があっても物理フレームが未割り当ての状態がありうる。
    pub allocated_frames: Vec<PhysFrame<Size4KiB>>,
    /// プロセスの仮想メモリ領域（VMA）リスト。
    /// ELF LOAD セグメント、ユーザースタック、mmap 領域を管理する。
    /// 空き仮想アドレスの検索や munmap での部分アンマップに使う。
    pub vma_list: crate::vma::VmaList,
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
        vma_list: crate::vma::VmaList::new(),
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
///
/// 注意: この変数はグローバルに1つしかないため、複数のユーザータスクが
/// jump_to_usermode() を呼ぶと互いに上書きしてしまう。
/// コンテキストスイッチ時にタスクごとにバックアップを取る必要がある。
/// → scheduler.rs の yield_now() / preempt() で退避・復帰している。
static mut SAVED_RSP: u64 = 0;
static mut SAVED_RBP: u64 = 0;

/// SAVED_RSP/SAVED_RBP の現在値を取得する。
/// コンテキストスイッチ前にタスクごとのバックアップに退避するために使う。
pub fn get_saved_usermode_context() -> (u64, u64) {
    unsafe { (SAVED_RSP, SAVED_RBP) }
}

/// SAVED_RSP/SAVED_RBP を設定する。
/// コンテキストスイッチ後にタスクごとのバックアップから復帰するために使う。
pub fn set_saved_usermode_context(rsp: u64, rbp: u64) {
    unsafe {
        SAVED_RSP = rsp;
        SAVED_RBP = rbp;
    }
}

/// ユーザーモード遷移時にレジスタ経由で渡す argc/argv/envp の値。
/// jump_to_usermode のアセンブリがこの値を rdi/rsi/rdx にセットしてから iretq する。
/// System V ABI 互換のレジスタ渡し:
///   rdi = argc（引数の数）
///   rsi = argv（引数ポインタ配列のアドレス）
///   rdx = envp（環境変数ポインタ配列のアドレス）
///
/// グローバル変数経由で渡す理由:
///   jump_to_usermode は既に 4 引数を取っており（Microsoft x64 ABI の上限）、
///   追加引数をスタック経由にすると ABI の複雑さが増す。
///   シングルコア + 割り込み無効状態で使うので、グローバル変数で安全に渡せる。
static mut USER_ENTRY_ARGC: u64 = 0;
static mut USER_ENTRY_ARGV: u64 = 0;
static mut USER_ENTRY_ENVP: u64 = 0;

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
//
// user_ss (SS) は 5番目の引数として渡さず、user_cs - 8 で計算する。
// GDT 配置が User Data → User Code の順なので、
// user_data_selector = user_code_selector - 8 が常に成り立つ。
// これにより、スタック上の5番目引数 [rsp+40] を読む必要がなくなり、
// コンパイラの最適化によるスタックレイアウトの違いに影響されない。
//
// 手順:
//   1. 現在の RSP/RBP を SAVED_RSP/SAVED_RBP に保存
//   2. user_ss を user_cs - 8 で計算
//   3. iretq 用スタックフレームを構築
//   4. iretq で Ring 3 に遷移
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

    // user_ss = user_cs - 8 で計算する。
    // GDT 配置: User Data (index 3) → User Code (index 4)
    // セレクタ値: User Data = 0x1b, User Code = 0x23
    // 差は常に 8 バイト（1 GDT エントリ分）。
    "mov rax, rdx",
    "sub rax, 8",

    // iretq 用スタックフレームを構築する。
    // iretq は以下の順で pop する:
    //   RIP → CS → RFLAGS → RSP → SS
    // なので push は逆順:
    "push rax",    // SS = user_ss (= user_cs - 8)
    "push r9",     // RSP = user_stack_top
    "push r8",     // RFLAGS (IF=1)
    "push rdx",    // CS = user_cs
    "push rcx",    // RIP = entry_addr

    // System V ABI 互換のレジスタ渡し:
    //   rdi = argc, rsi = argv, rdx = envp
    // グローバル変数から値をロードする。
    // これにより _start(argc, argv, envp) として引数を受け取れる。
    // 既存の _start() は引数なし宣言なので、レジスタ値を単に無視する。
    "mov rdi, [rip + {user_argc}]",
    "mov rsi, [rip + {user_argv}]",
    "mov rdx, [rip + {user_envp}]",

    "iretq",       // Ring 3 へ遷移！
    saved_rsp = sym SAVED_RSP,
    saved_rbp = sym SAVED_RBP,
    user_argc = sym USER_ENTRY_ARGC,
    user_argv = sym USER_ENTRY_ARGV,
    user_envp = sym USER_ENTRY_ENVP,
);

unsafe extern "C" {
    /// アセンブリで定義した Ring 3 遷移関数。
    /// Microsoft x64 ABI: jump_to_usermode(entry_addr, user_cs, rflags, user_stack_top)
    /// user_ss はアセンブリ内で user_cs - 8 から計算する。
    ///
    /// この関数は scheduler モジュールからも使用される（マルチプロセス対応）。
    pub fn jump_to_usermode(
        entry_addr: u64,
        user_cs: u64,
        rflags: u64,
        user_stack_top: u64,
    );
}

/// ユーザーモード遷移前に argc/argv/envp のレジスタ値を設定する。
///
/// jump_to_usermode のアセンブリがこの値を rdi/rsi/rdx にセットする。
/// 割り込み無効状態で呼ぶこと（シングルコアなのでグローバル変数で安全）。
///
/// # Safety
/// 割り込み無効状態で呼ぶ必要がある。
pub unsafe fn set_user_entry_args(argc: u64, argv: u64, envp: u64) {
    unsafe {
        USER_ENTRY_ARGC = argc;
        USER_ENTRY_ARGV = argv;
        USER_ENTRY_ENVP = envp;
    }
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
/// Ring 3 の例外（ページフォルト等）は例外ハンドラ側でユーザータスクを
/// 強制終了し、スケジューラが別タスクに切り替える。
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
    // iretq で push する CS の値。RPL=3 が含まれている。
    // SS は jump_to_usermode 内で CS - 8 から計算する。
    let user_cs = gdt::user_code_selector().0 as u64;

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
    // ページフォルトの場合は例外ハンドラがタスクを終了する。
    unsafe {
        jump_to_usermode(entry_addr, user_cs, rflags, user_stack_top);
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
/// page_fault_handler が USER_MODE ビットを検出してユーザータスクを終了する。
/// 結果的に run_in_usermode() が正常に return し、シェルに安全に戻る。
pub fn user_illegal_access() {
    // アドレス 0x0 はカーネル空間（USER_ACCESSIBLE なし）
    // Ring 3 からここを読もうとすると Page Fault が発生する。
    // → page_fault_handler がタスクを強制終了してカーネルに戻る。
    unsafe {
        core::ptr::read_volatile(0x0 as *const u8);
    }
}

// =================================================================
// ELF バイナリのロードと実行
// =================================================================
//
// include_bytes! で埋め込んだ ELF バイナリをパースし、
// PT_LOAD セグメントをプロセスのアドレス空間にロードして Ring 3 で実行する。
//
// 従来の create_user_process() + run_in_usermode() は、カーネルバイナリ内の
// Rust 関数をユーザーコードとして実行する方式だった。
// ELF ローダーでは外部バイナリを独立したアドレス空間にロードして実行する。
//
// ロードの流れ:
//   1. ELF パース（elf::parse_elf）でエントリポイントと LOAD セグメントを取得
//   2. プロセスページテーブルを作成
//   3. 各 LOAD セグメントについて:
//      a. map_user_pages_in_process() で物理フレームを確保しマッピング
//      b. アイデンティティマッピング経由で物理フレームにデータをコピー
//      c. BSS 領域（p_memsz > p_filesz の部分）はゼロ初期化
//   4. ユーザースタック用のフレームも確保してマッピング
//   5. iretq で Ring 3 に遷移

/// ELF ユーザースタックの仮想アドレス（0x800000 = 8MiB）。
/// ELF コードセクション（0x400000〜）とぶつからない位置に配置する。
const ELF_USER_STACK_VADDR: u64 = 0x2000000;

/// ELF ユーザースタックのサイズ（64KiB = 16ページ）。
const ELF_USER_STACK_SIZE: usize = 4096 * 16;

/// 埋め込み ELF バイナリのデータを返す。
///
/// シェルから ELF パース結果を表示するために使う。
pub fn get_user_elf_data() -> &'static [u8] {
    USER_ELF_DATA
}

/// ユーザースタック上に argc/argv/envp を配置するヘルパー関数。
///
/// スタックレイアウト（上位アドレスから下位アドレスへ）:
/// ```text
/// [High] 文字列データ ("arg0\0", "arg1\0", "KEY=VAL\0", ...)
///        envp[N] = NULL
///        envp[N-1] = pointer to env string
///        ...
///        envp[0] = pointer to first env string
///        argv[argc] = NULL
///        argv[argc-1] = pointer to last arg
///        ...
///        argv[0] = pointer to first arg (= program name)
/// [Low]  ← adjusted user_stack_top (RSP)
/// ```
///
/// # 引数
/// - `stack_top`: ユーザースタックの最上位アドレス
/// - `stack_frames`: スタック用の物理フレーム（アイデンティティマッピング経由で書き込む）
/// - `stack_vaddr`: スタックの仮想アドレス開始位置
/// - `args`: コマンドライン引数のスライス
/// - `env_vars`: 環境変数の "KEY=VALUE" 形式スライス
///
/// # 戻り値
/// (new_stack_top, argc, argv_addr, envp_addr)
fn setup_user_stack_args(
    stack_top: u64,
    stack_frames: &[PhysFrame<Size4KiB>],
    stack_vaddr: u64,
    args: &[&str],
    env_vars: &[String],
) -> (u64, u64, u64, u64) {
    // ステップ1: 文字列データをスタックの上位から書き込む
    // カーソルは stack_top から下に向かって進む
    let mut cursor = stack_top;

    // 文字列をスタックに書き込み、仮想アドレスを記録するヘルパー
    // アイデンティティマッピング経由で物理フレームに直接書き込む
    let write_to_stack = |vaddr: u64, data: &[u8], frames: &[PhysFrame<Size4KiB>], base_vaddr: u64| {
        for (i, &byte) in data.iter().enumerate() {
            let target_vaddr = vaddr + i as u64;
            let offset_from_base = (target_vaddr - base_vaddr) as usize;
            let frame_idx = offset_from_base / 4096;
            let offset_in_frame = offset_from_base % 4096;
            if frame_idx < frames.len() {
                let phys_ptr = (frames[frame_idx].start_address().as_u64() + offset_in_frame as u64) as *mut u8;
                unsafe { *phys_ptr = byte; }
            }
        }
    };

    // 各引数の文字列データを書き込み、仮想アドレスを記録
    let mut arg_addrs: Vec<u64> = Vec::with_capacity(args.len());
    for arg in args.iter().rev() {
        let len = arg.len() + 1; // null 終端分
        cursor -= len as u64;
        write_to_stack(cursor, arg.as_bytes(), stack_frames, stack_vaddr);
        // null 終端を書き込む
        write_to_stack(cursor + arg.len() as u64, &[0], stack_frames, stack_vaddr);
        arg_addrs.push(cursor);
    }
    arg_addrs.reverse(); // 正しい順序に戻す

    // 各環境変数の文字列データを書き込み
    let mut env_addrs: Vec<u64> = Vec::with_capacity(env_vars.len());
    for env in env_vars.iter().rev() {
        let len = env.len() + 1; // null 終端分
        cursor -= len as u64;
        write_to_stack(cursor, env.as_bytes(), stack_frames, stack_vaddr);
        write_to_stack(cursor + env.len() as u64, &[0], stack_frames, stack_vaddr);
        env_addrs.push(cursor);
    }
    env_addrs.reverse();

    // ステップ2: ポインタ配列を書き込む（8 バイトアラインメント）
    cursor = cursor & !0x7; // 8 バイトアラインに切り下げ

    // envp[N] = NULL
    cursor -= 8;
    write_to_stack(cursor, &0u64.to_le_bytes(), stack_frames, stack_vaddr);

    // envp[0..N-1] = 各環境変数文字列のアドレス（逆順に積む）
    for addr in env_addrs.iter().rev() {
        cursor -= 8;
        write_to_stack(cursor, &addr.to_le_bytes(), stack_frames, stack_vaddr);
    }
    let envp_addr = cursor;

    // argv[argc] = NULL
    cursor -= 8;
    write_to_stack(cursor, &0u64.to_le_bytes(), stack_frames, stack_vaddr);

    // argv[0..argc-1] = 各引数文字列のアドレス（逆順に積む）
    for addr in arg_addrs.iter().rev() {
        cursor -= 8;
        write_to_stack(cursor, &addr.to_le_bytes(), stack_frames, stack_vaddr);
    }
    let argv_addr = cursor;

    // 16 バイトアラインメントに揃える（System V ABI 要件）
    cursor = cursor & !0xF;

    let argc = args.len() as u64;
    (cursor, argc, argv_addr, envp_addr)
}

/// ELF バイナリからユーザープロセスを作成する。
///
/// 手順:
///   1. ELF パース: エントリポイントと LOAD セグメントを取得
///   2. プロセスページテーブルを作成（カーネルマッピングをコピー）
///   3. 各 LOAD セグメントを物理フレームにロード
///   4. ユーザースタック用の物理フレームを確保してマッピング
///   5. スタック上に argc/argv/envp を配置
///
/// 返り値: (UserProcess, entry_point, user_stack_top, argc, argv_addr, envp_addr)
///   - UserProcess: プロセスの状態（ページテーブル、カーネルスタック、確保フレーム）
///   - entry_point: ELF のエントリポイント仮想アドレス
///   - user_stack_top: ユーザースタックのトップアドレス（argc/argv/envp 配置後の調整済み RSP）
///   - argc: 引数の数
///   - argv_addr: argv 配列のユーザー空間アドレス
///   - envp_addr: envp 配列のユーザー空間アドレス
///
/// # 引数
/// - `elf_data`: ELF バイナリのデータ
/// - `args`: コマンドライン引数（args[0] はプログラム名）
/// - `env_vars`: 環境変数（"KEY=VALUE" 形式）
pub fn create_elf_process(
    elf_data: &[u8],
    args: &[&str],
    env_vars: &[String],
) -> Result<(UserProcess, u64, u64, u64, u64, u64), &'static str> {
    // 1. ELF パース
    let elf_info = crate::elf::parse_elf(elf_data)?;

    // 2. プロセスページテーブルを作成
    let page_table_frame = crate::paging::create_process_page_table();

    // 確保した全フレームを追跡するリスト
    let mut all_allocated_frames: Vec<PhysFrame<Size4KiB>> = Vec::new();

    // 3. 各 LOAD セグメントをプロセスのアドレス空間にロード
    for seg in &elf_info.load_segments {
        if seg.memsz == 0 {
            continue;
        }

        // 物理フレームを確保してプロセスのページテーブルにマッピング。
        // all_allocated_frames を渡すことで、前のセグメントで既にマッピング済みの
        // ページ（同じページに複数セグメントがある場合）を再利用する。
        let frames = crate::paging::map_user_pages_in_process(
            page_table_frame,
            VirtAddr::new(seg.vaddr),
            seg.memsz as usize,
            &all_allocated_frames,
            seg.flags, // ELF セグメントのパーミッションを渡す（W^X 適用）
        );

        // セグメントのファイルデータを物理フレームにコピーする。
        // アイデンティティマッピング（仮想アドレス == 物理アドレス）のおかげで、
        // 確保したフレームの物理アドレスをそのままポインタとして使える。
        // CR3 を切り替える必要がないので、カーネルのページテーブルのまま書き込める。
        if seg.filesz > 0 {
            let src = &elf_data[seg.offset as usize..(seg.offset + seg.filesz) as usize];
            let mut remaining = src;
            let page_base = seg.vaddr & !0xFFF; // ページアラインされた開始アドレス
            let offset_in_first_page = (seg.vaddr - page_base) as usize;

            for (i, frame) in frames.iter().enumerate() {
                // フレームの物理アドレス = アイデンティティマッピングでの仮想アドレス
                let frame_ptr = frame.start_address().as_u64() as *mut u8;

                // 最初のフレームではページ内オフセットを考慮する。
                // 例: vaddr=0x400040 の場合、フレームは 0x400000 にマッピングされるが、
                // データは 0x40 バイト目から書き込む。
                let start_offset = if i == 0 { offset_in_first_page } else { 0 };
                let available = 4096 - start_offset;
                let to_copy = remaining.len().min(available);

                if to_copy > 0 {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            remaining.as_ptr(),
                            frame_ptr.add(start_offset),
                            to_copy,
                        );
                    }
                    remaining = &remaining[to_copy..];
                }

                if remaining.is_empty() {
                    break;
                }
            }
        }

        // BSS 領域（p_memsz > p_filesz の部分）は map_user_pages_in_process() が
        // 確保時にゼロクリア済みなので、追加の処理は不要。

        // フレームリストに追加する（重複を除く）。
        // 同じページに複数の LOAD セグメントがまたがる場合、map_user_pages_in_process() が
        // 既存マッピングのフレームを返すため、重複が発生しうる。
        // 重複フレームを解放リストに入れると二重解放になるので除去する。
        for frame in &frames {
            if !all_allocated_frames.iter().any(|f| f.start_address() == frame.start_address()) {
                all_allocated_frames.push(*frame);
            }
        }
    }

    // 4. ユーザースタック用のフレームを確保してマッピング
    //    スタックは 0x2000000 (32MiB) に 16KiB 分確保する。
    //    スタックは高アドレスから低アドレスに向かって伸びるので、
    //    スタックトップは ELF_USER_STACK_VADDR + ELF_USER_STACK_SIZE。
    // スタックは書き込み可能・実行不可（W^X: NX ビットを設定）
    // PF_R=4, PF_W=2 → WRITABLE あり・NO_EXECUTE あり
    const STACK_FLAGS: u32 = 4 | 2; // PF_R | PF_W（実行不可）
    let stack_frames = crate::paging::map_user_pages_in_process(
        page_table_frame,
        VirtAddr::new(ELF_USER_STACK_VADDR),
        ELF_USER_STACK_SIZE,
        &all_allocated_frames,
        STACK_FLAGS,
    );
    // スタックフレームは新規確保なので重複の心配なし
    all_allocated_frames.extend_from_slice(&stack_frames);

    // 5. ユーザースタック上に argc/argv/envp を配置する
    //    スタックは高アドレスから低アドレスに伸びるので、
    //    文字列データ → ポインタ配列 の順で上位から配置し、
    //    最終的な RSP をそこに設定する。
    let raw_stack_top = ELF_USER_STACK_VADDR + ELF_USER_STACK_SIZE as u64;
    let (adjusted_stack_top, argc, argv_addr, envp_addr) = setup_user_stack_args(
        raw_stack_top,
        &stack_frames,
        ELF_USER_STACK_VADDR,
        args,
        env_vars,
    );

    // 6. VMA リストを構築する
    //    ELF の各 LOAD セグメントとユーザースタックを VMA として登録する。
    //    これにより mmap での空き領域検索や /proc/maps での表示に使える。
    let mut vma_list = crate::vma::VmaList::new();
    for seg in &elf_info.load_segments {
        if seg.memsz == 0 {
            continue;
        }
        // ページアラインされた範囲を VMA として登録
        let page_start = seg.vaddr & !0xFFF;
        let page_end = (seg.vaddr + seg.memsz + 0xFFF) & !0xFFF;
        // セグメント名を ELF フラグから推定
        let name = if seg.flags & 1 != 0 {
            // 実行可能 → .text
            String::from(".text")
        } else if seg.flags & 2 != 0 {
            // 書き込み可能 → .data/.bss
            String::from(".data")
        } else {
            // 読み取り専用 → .rodata
            String::from(".rodata")
        };
        let _ = vma_list.insert(crate::vma::Vma {
            start: page_start,
            end: page_end,
            prot: crate::vma::VmaProt::from_elf_flags(seg.flags),
            kind: crate::vma::VmaKind::ElfLoad,
            name,
        });
    }
    // ユーザースタックの VMA を登録
    let _ = vma_list.insert(crate::vma::Vma {
        start: ELF_USER_STACK_VADDR,
        end: ELF_USER_STACK_VADDR + ELF_USER_STACK_SIZE as u64,
        prot: crate::vma::VmaProt::read_write(),
        kind: crate::vma::VmaKind::UserStack,
        name: String::from("[stack]"),
    });

    // 7. カーネルスタックを確保（プロセスごとに独立）
    let kernel_stack = alloc::vec![0u8; KERNEL_STACK_SIZE].into_boxed_slice();

    let process = UserProcess {
        page_table_frame,
        kernel_stack,
        allocated_frames: all_allocated_frames,
        vma_list,
    };

    let entry_point = elf_info.entry_point;

    Ok((process, entry_point, adjusted_stack_top, argc, argv_addr, envp_addr))
}

/// ELF プロセスを Ring 3 で実行する。
///
/// create_elf_process() で作成したプロセスを実行する。
/// run_in_usermode() と同様の手順だが、エントリポイントとスタックアドレスが
/// ELF バイナリに基づく固定アドレス（カーネルバイナリ内の関数ポインタではない）。
///
/// # 引数
/// - process: ELF プロセス
/// - entry_point: ELF のエントリポイントアドレス
/// - user_stack_top: ユーザースタックのトップアドレス
pub fn run_elf_process(process: &UserProcess, entry_point: u64, user_stack_top: u64) {
    // カーネルスタックのトップアドレスを計算
    let kernel_stack_top = {
        let start = process.kernel_stack.as_ptr() as u64;
        start + process.kernel_stack.len() as u64
    };

    // TSS rsp0 にカーネルスタックのトップを設定する
    unsafe {
        gdt::set_tss_rsp0(VirtAddr::new(kernel_stack_top));
    }

    // セグメントセレクタを取得
    // SS は jump_to_usermode 内で CS - 8 から計算する。
    let user_cs = gdt::user_code_selector().0 as u64;

    // RFLAGS: IF (Interrupt Flag, bit 9) を立てておく
    let rflags: u64 = 0x200;

    // CR3 をプロセスのページテーブルに切り替え
    unsafe {
        crate::paging::switch_to_process_page_table(process.page_table_frame);
    }

    // Ring 3 に遷移
    unsafe {
        jump_to_usermode(entry_point, user_cs, rflags, user_stack_top);
    }

    // exit_usermode() 経由で Ring 3 から戻ってきた

    // CR3 をカーネルのページテーブルに復帰
    unsafe {
        crate::paging::switch_to_kernel_page_table();
    }
}
