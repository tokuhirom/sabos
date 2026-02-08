// sys/fs/sabos.rs — SABOS ファイルシステム PAL 実装
//
// SABOS のハンドルベース syscall (SYS_OPEN=70, SYS_HANDLE_READ=71, SYS_HANDLE_WRITE=72,
// SYS_HANDLE_CLOSE=73, SYS_HANDLE_STAT=77, SYS_HANDLE_SEEK=78) と、
// パスベース syscall (SYS_FILE_DELETE=12, SYS_DIR_CREATE=15, SYS_DIR_REMOVE=16,
// SYS_DIR_LIST=13) を使って std::fs のインターフェースを実装する。
//
// unsupported.rs をベースに、SABOS で実装可能な操作だけ syscall に接続。
// リンク関連やパーミッション変更など SABOS 未対応の操作は unsupported() を返す。

use crate::ffi::OsString;
use crate::fmt;
use crate::fs::TryLockError;
use crate::hash::{Hash, Hasher};
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut, SeekFrom};
use crate::os::sabos::ffi::OsStringExt;
use crate::path::{Path, PathBuf};
use crate::sys::time::SystemTime;

// ============================================================
// SABOS syscall 番号の定義
// ============================================================
const SYS_FILE_DELETE: u64 = 12;
const SYS_DIR_LIST: u64 = 13;
const SYS_FILE_WRITE: u64 = 14;
const SYS_DIR_CREATE: u64 = 15;
const SYS_DIR_REMOVE: u64 = 16;
const SYS_OPEN: u64 = 70;
const SYS_HANDLE_READ: u64 = 71;
const SYS_HANDLE_WRITE: u64 = 72;
const SYS_HANDLE_CLOSE: u64 = 73;
const SYS_HANDLE_STAT: u64 = 77;
const SYS_HANDLE_SEEK: u64 = 78;

// ハンドルの権限ビット
const HANDLE_RIGHTS_FILE_READ: u32 = 0x000D; // READ | SEEK | STAT
const HANDLE_RIGHTS_FILE_RW: u32 = 0x000F; // READ | WRITE | SEEK | STAT

// seek の whence 定数
const SEEK_SET: u64 = 0;
const SEEK_CUR: u64 = 1;
const SEEK_END: u64 = 2;

// HandleStat の kind 定数
const HANDLE_KIND_FILE: u64 = 0;
const HANDLE_KIND_DIRECTORY: u64 = 1;

// ============================================================
// SABOS ハンドル構造体 (カーネルの Handle と同じレイアウト)
// ============================================================
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
struct SabosHandle {
    id: u64,
    token: u64,
}

impl SabosHandle {
    const INVALID: SabosHandle = SabosHandle { id: 0, token: 0 };
}

// カーネルの HandleStat と同じレイアウト
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct SabosHandleStat {
    size: u64,
    kind: u64,
    rights: u64,
}

// ============================================================
// syscall ヘルパー関数
// ============================================================

/// syscall の戻り値（負の値）を io::Error に変換する
fn errno_to_io_error(errno: i64) -> io::Error {
    match errno {
        -20 => io::Error::new(io::ErrorKind::NotFound, "file not found"),
        -21 => io::Error::new(io::ErrorKind::InvalidInput, "invalid handle"),
        -22 => io::Error::new(
            io::ErrorKind::PermissionDenied,
            "read-only filesystem or file",
        ),
        -30 => io::Error::new(io::ErrorKind::PermissionDenied, "permission denied"),
        -10 => io::Error::new(io::ErrorKind::InvalidInput, "invalid argument"),
        -11 => io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-8"),
        -41 => io::Error::new(io::ErrorKind::Unsupported, "not supported"),
        _ => io::Error::new(io::ErrorKind::Other, "syscall error"),
    }
}

/// syscall の戻り値をチェックして、エラーなら io::Error に変換する
fn check_syscall_result(ret: u64) -> io::Result<u64> {
    let signed = ret as i64;
    if signed < 0 {
        Err(errno_to_io_error(signed))
    } else {
        Ok(ret)
    }
}

/// SYS_OPEN(70): ファイルをオープンしてハンドルを取得する
fn syscall_open(path: &[u8], rights: u32) -> io::Result<SabosHandle> {
    let mut handle = SabosHandle::INVALID;
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_OPEN,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            in("rdx") &mut handle as *mut SabosHandle as u64,
            in("r10") rights as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    check_syscall_result(ret)?;
    Ok(handle)
}

/// SYS_HANDLE_READ(71): ハンドルからデータを読み取る
fn syscall_handle_read(h: &SabosHandle, buf: &mut [u8]) -> io::Result<usize> {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_HANDLE_READ,
            in("rdi") h as *const SabosHandle as u64,
            in("rsi") buf.as_mut_ptr() as u64,
            in("rdx") buf.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    check_syscall_result(ret).map(|n| n as usize)
}

/// SYS_HANDLE_WRITE(72): ハンドルにデータを書き込む
fn syscall_handle_write(h: &SabosHandle, buf: &[u8]) -> io::Result<usize> {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_HANDLE_WRITE,
            in("rdi") h as *const SabosHandle as u64,
            in("rsi") buf.as_ptr() as u64,
            in("rdx") buf.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    check_syscall_result(ret).map(|n| n as usize)
}

/// SYS_HANDLE_CLOSE(73): ハンドルをクローズする
fn syscall_handle_close(h: &SabosHandle) -> io::Result<()> {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_HANDLE_CLOSE,
            in("rdi") h as *const SabosHandle as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    check_syscall_result(ret)?;
    Ok(())
}

/// SYS_HANDLE_STAT(77): ハンドルのメタデータを取得する
fn syscall_handle_stat(h: &SabosHandle) -> io::Result<SabosHandleStat> {
    let mut stat = SabosHandleStat {
        size: 0,
        kind: 0,
        rights: 0,
    };
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_HANDLE_STAT,
            in("rdi") h as *const SabosHandle as u64,
            in("rsi") &mut stat as *mut SabosHandleStat as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    check_syscall_result(ret)?;
    Ok(stat)
}

/// SYS_HANDLE_SEEK(78): ファイルポジションを変更する
fn syscall_handle_seek(h: &SabosHandle, offset: i64, whence: u64) -> io::Result<u64> {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_HANDLE_SEEK,
            in("rdi") h as *const SabosHandle as u64,
            in("rsi") offset as u64,
            in("rdx") whence,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    check_syscall_result(ret)
}

/// SYS_FILE_DELETE(12): パスを指定してファイルを削除する
fn syscall_file_delete(path: &[u8]) -> io::Result<()> {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_FILE_DELETE,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    check_syscall_result(ret)?;
    Ok(())
}

/// SYS_DIR_CREATE(15): ディレクトリを作成する
fn syscall_dir_create(path: &[u8]) -> io::Result<()> {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_DIR_CREATE,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    check_syscall_result(ret)?;
    Ok(())
}

/// SYS_DIR_REMOVE(16): ディレクトリを削除する
fn syscall_dir_remove(path: &[u8]) -> io::Result<()> {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_DIR_REMOVE,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    check_syscall_result(ret)?;
    Ok(())
}

/// SYS_DIR_LIST(13): ディレクトリの内容一覧を取得する
/// 改行区切りのエントリ名が返る。ディレクトリは末尾に "/" が付く。
fn syscall_dir_list(path: &[u8], buf: &mut [u8]) -> io::Result<usize> {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_DIR_LIST,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            in("rdx") buf.as_mut_ptr() as u64,
            in("r10") buf.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    check_syscall_result(ret).map(|n| n as usize)
}

/// SYS_FILE_WRITE(14): パスベースでファイルにデータを書き込む（新規作成/上書き）
fn syscall_file_write(path: &[u8], data: &[u8]) -> io::Result<()> {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_FILE_WRITE,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            in("rdx") data.as_ptr() as u64,
            in("r10") data.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    check_syscall_result(ret)?;
    Ok(())
}

// ============================================================
// パスを &[u8] に変換するヘルパー
// ============================================================
fn path_to_bytes(path: &Path) -> &[u8] {
    use crate::os::sabos::ffi::OsStrExt;
    path.as_os_str().as_bytes()
}

// ============================================================
// unsupported ヘルパー
// ============================================================
fn unsupported<T>() -> io::Result<T> {
    Err(io::Error::UNSUPPORTED_PLATFORM)
}

// ============================================================
// 公開型の定義
// ============================================================

/// ファイルハンドル: SABOS の Handle を内部に保持する
pub struct File {
    handle: SabosHandle,
}

/// ファイル属性: サイズと種別を保持する
#[derive(Clone)]
pub struct FileAttr {
    size: u64,
    kind: u64,
}

/// ディレクトリ読み取りイテレータ
pub struct ReadDir {
    // パースしたエントリのリストと現在位置
    entries: crate::vec::Vec<DirEntry>,
    pos: usize,
}

/// ディレクトリエントリ
pub struct DirEntry {
    // エントリのフルパス
    entry_path: PathBuf,
    // エントリの名前
    name: OsString,
    // 種別: 0=File, 1=Directory
    kind: u64,
}

/// ファイルオープンオプション
#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

/// ファイルタイムスタンプ（SABOS は未対応だが型は必要）
#[derive(Copy, Clone, Debug, Default)]
pub struct FileTimes {}

/// ファイルパーミッション
#[derive(Clone, PartialEq, Eq)]
pub struct FilePermissions {
    readonly: bool,
}

/// ファイルタイプ
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FileType {
    kind: u64,
}

/// ディレクトリビルダー
#[derive(Debug)]
pub struct DirBuilder {}

/// Dir 型: common.rs の実装を使う
pub use crate::sys::fs::common::Dir;

// ============================================================
// FileAttr の実装
// ============================================================
impl FileAttr {
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn perm(&self) -> FilePermissions {
        // SABOS は詳細なパーミッション未対応。読み取り可能として返す
        FilePermissions { readonly: false }
    }

    pub fn file_type(&self) -> FileType {
        FileType { kind: self.kind }
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        // SABOS はタイムスタンプ未対応
        unsupported()
    }

    pub fn accessed(&self) -> io::Result<SystemTime> {
        unsupported()
    }

    pub fn created(&self) -> io::Result<SystemTime> {
        unsupported()
    }
}

// ============================================================
// FilePermissions の実装
// ============================================================
impl FilePermissions {
    pub fn readonly(&self) -> bool {
        self.readonly
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }
}

impl fmt::Debug for FilePermissions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilePermissions")
            .field("readonly", &self.readonly)
            .finish()
    }
}

// ============================================================
// FileTimes の実装
// ============================================================
impl FileTimes {
    pub fn set_accessed(&mut self, _t: SystemTime) {}
    pub fn set_modified(&mut self, _t: SystemTime) {}
}

// ============================================================
// FileType の実装
// ============================================================
impl FileType {
    pub fn is_dir(&self) -> bool {
        self.kind == HANDLE_KIND_DIRECTORY
    }

    pub fn is_file(&self) -> bool {
        self.kind == HANDLE_KIND_FILE
    }

    pub fn is_symlink(&self) -> bool {
        // SABOS はシンボリックリンク未対応
        false
    }
}

impl Hash for FileType {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.kind.hash(h);
    }
}

impl fmt::Debug for FileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.kind {
            HANDLE_KIND_FILE => "File",
            HANDLE_KIND_DIRECTORY => "Directory",
            _ => "Unknown",
        };
        f.debug_struct("FileType").field("kind", &name).finish()
    }
}

// ============================================================
// ReadDir の実装
// ============================================================
impl fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadDir")
            .field("entries_count", &self.entries.len())
            .field("pos", &self.pos)
            .finish()
    }
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<io::Result<DirEntry>> {
        if self.pos < self.entries.len() {
            // エントリを取得して pos を進める
            // entries から直接取り出すために swap_remove は使わず、
            // Vec の中身を一つずつ消費するために drain 的な処理をする
            let entry = &self.entries[self.pos];
            let result = DirEntry {
                entry_path: entry.entry_path.clone(),
                name: entry.name.clone(),
                kind: entry.kind,
            };
            self.pos += 1;
            Some(Ok(result))
        } else {
            None
        }
    }
}

// ============================================================
// DirEntry の実装
// ============================================================
impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.entry_path.clone()
    }

    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }

    pub fn metadata(&self) -> io::Result<FileAttr> {
        // パスベースで stat を呼ぶ
        stat(&self.entry_path)
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(FileType { kind: self.kind })
    }
}

// ============================================================
// OpenOptions の実装
// ============================================================
impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
        }
    }

    pub fn read(&mut self, read: bool) {
        self.read = read;
    }
    pub fn write(&mut self, write: bool) {
        self.write = write;
    }
    pub fn append(&mut self, append: bool) {
        self.append = append;
    }
    pub fn truncate(&mut self, truncate: bool) {
        self.truncate = truncate;
    }
    pub fn create(&mut self, create: bool) {
        self.create = create;
    }
    pub fn create_new(&mut self, create_new: bool) {
        self.create_new = create_new;
    }
}

// ============================================================
// File の実装
// ============================================================
impl File {
    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<File> {
        let path_bytes = path_to_bytes(path);

        // truncate=true の場合: 空データで上書きしてから open する
        if opts.truncate && opts.write {
            syscall_file_write(path_bytes, &[])?;
        }

        // create=true + write=true の場合:
        // SABOS の SYS_OPEN は WRITE 権限付きならファイルが無くても新規作成する。
        // ただし create_new の場合、既存ファイルがあればエラーにしたい。
        if opts.create_new {
            // まず open して存在確認 → 存在していたらエラー
            if let Ok(h) = syscall_open(path_bytes, HANDLE_RIGHTS_FILE_READ) {
                let _ = syscall_handle_close(&h);
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "file already exists",
                ));
            }
        }

        // 権限の決定
        let rights = if opts.write || opts.append || opts.create || opts.create_new {
            HANDLE_RIGHTS_FILE_RW
        } else {
            HANDLE_RIGHTS_FILE_READ
        };

        let handle = syscall_open(path_bytes, rights)?;

        // append モードの場合、末尾にシークする
        if opts.append {
            let _ = syscall_handle_seek(&handle, 0, SEEK_END);
        }

        Ok(File { handle })
    }

    pub fn file_attr(&self) -> io::Result<FileAttr> {
        let stat = syscall_handle_stat(&self.handle)?;
        Ok(FileAttr {
            size: stat.size,
            kind: stat.kind,
        })
    }

    pub fn fsync(&self) -> io::Result<()> {
        // SABOS はクローズ時にフラッシュするため、fsync は何もしない
        Ok(())
    }

    pub fn datasync(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn lock(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn lock_shared(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn try_lock(&self) -> Result<(), TryLockError> {
        Err(TryLockError::Error(io::Error::UNSUPPORTED_PLATFORM))
    }

    pub fn try_lock_shared(&self) -> Result<(), TryLockError> {
        Err(TryLockError::Error(io::Error::UNSUPPORTED_PLATFORM))
    }

    pub fn unlock(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn truncate(&self, _size: u64) -> io::Result<()> {
        unsupported()
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        syscall_handle_read(&self.handle, buf)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        // 最初の非空バッファだけ読む（scatter read は未対応）
        for buf in bufs {
            if !buf.is_empty() {
                return self.read(buf);
            }
        }
        Ok(0)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn read_buf(&self, mut cursor: BorrowedCursor<'_>) -> io::Result<()> {
        let buf = cursor.ensure_init();
        let n = syscall_handle_read(&self.handle, buf.init_mut())?;
        cursor.advance(n);
        Ok(())
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        syscall_handle_write(&self.handle, buf)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for buf in bufs {
            if !buf.is_empty() {
                let n = self.write(buf)?;
                total += n;
                if n < buf.len() {
                    break;
                }
            }
        }
        Ok(total)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn flush(&self) -> io::Result<()> {
        // SABOS はクローズ時にフラッシュする
        Ok(())
    }

    pub fn seek(&self, pos: SeekFrom) -> io::Result<u64> {
        let (offset, whence) = match pos {
            SeekFrom::Start(n) => (n as i64, SEEK_SET),
            SeekFrom::Current(n) => (n, SEEK_CUR),
            SeekFrom::End(n) => (n, SEEK_END),
        };
        syscall_handle_seek(&self.handle, offset, whence)
    }

    pub fn size(&self) -> Option<io::Result<u64>> {
        Some(self.file_attr().map(|a| a.size))
    }

    pub fn tell(&self) -> io::Result<u64> {
        syscall_handle_seek(&self.handle, 0, SEEK_CUR)
    }

    pub fn duplicate(&self) -> io::Result<File> {
        unsupported()
    }

    pub fn set_permissions(&self, _perm: FilePermissions) -> io::Result<()> {
        unsupported()
    }

    pub fn set_times(&self, _times: FileTimes) -> io::Result<()> {
        unsupported()
    }
}

impl Drop for File {
    fn drop(&mut self) {
        // ハンドルをクローズする（エラーは無視）
        let _ = syscall_handle_close(&self.handle);
    }
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("File")
            .field("handle_id", &self.handle.id)
            .finish()
    }
}

// ============================================================
// DirBuilder の実装
// ============================================================
impl DirBuilder {
    pub fn new() -> DirBuilder {
        DirBuilder {}
    }

    pub fn mkdir(&self, p: &Path) -> io::Result<()> {
        syscall_dir_create(path_to_bytes(p))
    }
}

// ============================================================
// モジュールレベル関数
// ============================================================

/// ディレクトリの内容をイテレータとして返す
pub fn readdir(p: &Path) -> io::Result<ReadDir> {
    let path_bytes = path_to_bytes(p);

    // バッファを用意して SYS_DIR_LIST を呼ぶ
    let mut buf = crate::vec![0u8; 4096];
    let n = syscall_dir_list(path_bytes, &mut buf)?;
    let data = &buf[..n];

    // 改行区切りでパースする
    // ディレクトリ名は末尾に "/" が付いている
    let text = core::str::from_utf8(data)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-8 in dir listing"))?;

    let mut entries = crate::vec::Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }

        let (name, kind) = if let Some(dir_name) = line.strip_suffix('/') {
            (dir_name, HANDLE_KIND_DIRECTORY)
        } else {
            (line, HANDLE_KIND_FILE)
        };

        // フルパスを構築
        let entry_path = p.join(name);
        let os_name = OsString::from_vec(name.as_bytes().to_vec());

        entries.push(DirEntry {
            entry_path,
            name: os_name,
            kind,
        });
    }

    Ok(ReadDir { entries, pos: 0 })
}

/// ファイルを削除する
pub fn unlink(p: &Path) -> io::Result<()> {
    syscall_file_delete(path_to_bytes(p))
}

/// ファイル名を変更する（SABOS 未対応）
pub fn rename(_old: &Path, _new: &Path) -> io::Result<()> {
    unsupported()
}

/// パーミッションを設定する（SABOS 未対応）
pub fn set_perm(_p: &Path, _perm: FilePermissions) -> io::Result<()> {
    unsupported()
}

/// タイムスタンプを設定する（SABOS 未対応）
pub fn set_times(_p: &Path, _times: FileTimes) -> io::Result<()> {
    unsupported()
}

/// タイムスタンプを設定する（シンボリックリンクを辿らない版、SABOS 未対応）
pub fn set_times_nofollow(_p: &Path, _times: FileTimes) -> io::Result<()> {
    unsupported()
}

/// ディレクトリを削除する
pub fn rmdir(p: &Path) -> io::Result<()> {
    syscall_dir_remove(path_to_bytes(p))
}

/// ディレクトリを再帰的に削除する
/// common.rs の実装を使う
pub use crate::sys::fs::common::remove_dir_all;

/// ファイルまたはディレクトリが存在するか確認する
/// common.rs の exists を使う（stat → NotFound で判定）
pub use crate::sys::fs::common::exists;

/// シンボリックリンクの先を読む（SABOS 未対応）
pub fn readlink(_p: &Path) -> io::Result<PathBuf> {
    unsupported()
}

/// シンボリックリンクを作成する（SABOS 未対応）
pub fn symlink(_original: &Path, _link: &Path) -> io::Result<()> {
    unsupported()
}

/// ハードリンクを作成する（SABOS 未対応）
pub fn link(_src: &Path, _dst: &Path) -> io::Result<()> {
    unsupported()
}

/// ファイルのメタデータを取得する
/// ファイルを open → stat → close してメタデータを返す
pub fn stat(p: &Path) -> io::Result<FileAttr> {
    let path_bytes = path_to_bytes(p);
    let handle = syscall_open(path_bytes, HANDLE_RIGHTS_FILE_READ)?;
    let stat = syscall_handle_stat(&handle);
    let _ = syscall_handle_close(&handle);
    let stat = stat?;
    Ok(FileAttr {
        size: stat.size,
        kind: stat.kind,
    })
}

/// シンボリックリンクを辿らない版の stat（SABOS にはシンボリックリンクがないので stat と同じ）
pub fn lstat(p: &Path) -> io::Result<FileAttr> {
    stat(p)
}

/// パスを正規化する（SABOS 未対応）
pub fn canonicalize(_p: &Path) -> io::Result<PathBuf> {
    unsupported()
}

/// ファイルをコピーする
/// common.rs の実装を使う
pub use crate::sys::fs::common::copy;
