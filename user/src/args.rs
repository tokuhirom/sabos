// args.rs — コマンドライン引数と環境変数のヘルパー
//
// _start(argc, argv, envp) で受け取った値をパースして
// 使いやすい API を提供する。
//
// スタック上に配置されたデータ構造:
//   argv[0]..argv[argc-1]: null 終端 C 文字列へのポインタ
//   argv[argc] = NULL
//   envp[0]..envp[N-1]: "KEY=VALUE\0" 形式の C 文字列へのポインタ
//   envp[N] = NULL
//
// レジスタ渡し（System V ABI 互換）:
//   rdi = argc
//   rsi = argv (argv[0] のアドレス)
//   rdx = envp (envp[0] のアドレス)

// 公開 API は外部のユーザープログラムから使用されるため、dead_code 警告を抑制
#![allow(dead_code)]

/// 保存された argc/argv/envp のグローバル変数。
/// _start() の最初で init() を呼んで設定する。
static mut ARGC: usize = 0;
static mut ARGV: *const *const u8 = core::ptr::null();
static mut ENVP: *const *const u8 = core::ptr::null();

/// argc/argv/envp を初期化する。
///
/// _start(argc, argv, envp) の先頭で呼ぶこと。
/// グローバル変数に保存して、以降は argc() / argv() / getenv() で参照する。
///
/// # Safety
/// - argv と envp は有効なポインタ配列であること
/// - argv は argc 個のポインタ + NULL 終端であること
/// - envp は NULL 終端のポインタ配列であること
pub unsafe fn init(argc: usize, argv: *const *const u8, envp: *const *const u8) {
    unsafe {
        ARGC = argc;
        ARGV = argv;
        ENVP = envp;
    }
}

/// コマンドライン引数の数を返す。
pub fn argc() -> usize {
    unsafe { ARGC }
}

/// 指定したインデックスのコマンドライン引数を返す。
///
/// argv[index] の null 終端 C 文字列を &str として返す。
/// インデックスが範囲外の場合は None を返す。
pub fn argv(index: usize) -> Option<&'static str> {
    unsafe {
        if index >= ARGC || ARGV.is_null() {
            return None;
        }
        let ptr = *ARGV.add(index);
        if ptr.is_null() {
            return None;
        }
        // null 終端文字列の長さを測る
        let len = c_str_len(ptr);
        let bytes = core::slice::from_raw_parts(ptr, len);
        core::str::from_utf8(bytes).ok()
    }
}

/// 環境変数を検索する。
///
/// envp[] から "KEY=VALUE" 形式の文字列を探し、
/// key に一致するエントリの VALUE 部分を返す。
pub fn getenv(key: &str) -> Option<&'static str> {
    unsafe {
        if ENVP.is_null() {
            return None;
        }
        let mut i = 0;
        loop {
            let ptr = *ENVP.add(i);
            if ptr.is_null() {
                break;
            }
            let len = c_str_len(ptr);
            let bytes = core::slice::from_raw_parts(ptr, len);
            if let Ok(entry) = core::str::from_utf8(bytes) {
                // "KEY=VALUE" 形式を分割
                if let Some(eq_pos) = entry.find('=') {
                    let entry_key = &entry[..eq_pos];
                    if entry_key == key {
                        return Some(&entry[eq_pos + 1..]);
                    }
                }
            }
            i += 1;
        }
        None
    }
}

/// null 終端 C 文字列の長さを返す（null バイトを含まない）。
unsafe fn c_str_len(ptr: *const u8) -> usize {
    let mut len = 0;
    unsafe {
        while *ptr.add(len) != 0 {
            len += 1;
        }
    }
    len
}
