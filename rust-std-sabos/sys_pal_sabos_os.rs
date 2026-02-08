// sys/pal/sabos/os.rs — SABOS OS 関数
//
// unsupported/os.rs をベースに、SABOS 向けの OS 関数を実装。
// - exit() / getpid(): SABOS システムコール経由
// - getcwd(): SABOS はフラット FAT32 なのでルート "/" を返す
// - temp_dir(): ファイルシステムのルート "/" を返す
// - home_dir(): ルート "/" を返す
// - chdir() / current_exe(): unsupported（カーネル側に未実装）

use super::unsupported;
use crate::ffi::{OsStr, OsString};
use crate::marker::PhantomData;
use crate::path::{self, PathBuf};
use crate::{fmt, io};

/// カレントディレクトリを取得する。
/// SABOS はフラット FAT32 ファイルシステムで、ディレクトリ階層の概念が薄いため
/// 常にルート "/" を返す。将来 chdir を実装したら対応する。
pub fn getcwd() -> io::Result<PathBuf> {
    Ok(PathBuf::from("/"))
}

/// カレントディレクトリを変更する。
/// SABOS カーネルに cwd の概念がまだないため unsupported。
pub fn chdir(_: &path::Path) -> io::Result<()> {
    unsupported()
}

pub struct SplitPaths<'a>(!, PhantomData<&'a ()>);

pub fn split_paths(_unparsed: &OsStr) -> SplitPaths<'_> {
    panic!("unsupported")
}

impl<'a> Iterator for SplitPaths<'a> {
    type Item = PathBuf;
    fn next(&mut self) -> Option<PathBuf> {
        self.0
    }
}

#[derive(Debug)]
pub struct JoinPathsError;

pub fn join_paths<I, T>(_paths: I) -> Result<OsString, JoinPathsError>
where
    I: Iterator<Item = T>,
    T: AsRef<OsStr>,
{
    Err(JoinPathsError)
}

impl fmt::Display for JoinPathsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "not supported on SABOS yet".fmt(f)
    }
}

impl crate::error::Error for JoinPathsError {}

/// 実行中のバイナリのパスを取得する。
/// カーネルが実行パスを保持していないため unsupported。
pub fn current_exe() -> io::Result<PathBuf> {
    unsupported()
}

/// テンポラリディレクトリを返す。
/// SABOS ではルートディレクトリ "/" を返す。
pub fn temp_dir() -> PathBuf {
    PathBuf::from("/")
}

/// ホームディレクトリを返す。
/// SABOS ではルートディレクトリ "/" を返す。
pub fn home_dir() -> Option<PathBuf> {
    Some(PathBuf::from("/"))
}

/// プロセスを終了する: SYS_EXIT(60) を呼ぶ
pub fn exit(code: i32) -> ! {
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 60u64,   // SYS_EXIT
            in("rdi") code as u64,
            options(noreturn)
        );
    }
}

/// プロセス ID を取得する: SYS_GETPID(35) を呼ぶ
pub fn getpid() -> u32 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 35u64,   // SYS_GETPID
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as u32
}
