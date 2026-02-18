// sys/process/sabos.rs — SABOS プロセス管理 PAL 実装
//
// SYS_SPAWN(31) / SYS_SPAWN_REDIRECTED(6) / SYS_WAIT(34) / SYS_KILL(36) を使って
// std::process::Command を実装する。
//
// ## パイプ対応
//
// stdout/stdin の MakePipe が指定された場合:
// 1. SYS_PIPE(5) でパイプハンドルペアを作成
// 2. SYS_SPAWN_REDIRECTED(6) でパイプの一端を子プロセスに渡す
// 3. 親プロセスでもう一端を使って I/O を行う
// 4. 親で不要な端を drop することで、子プロセス終了時に EOF が発生する
//
// stderr リダイレクトは未対応（SABOS の SpawnRedirectArgs は stdin/stdout のみ）。
//
// ## 引数バッファ形式
//
// SABOS は null 終端文字列を使わない。引数バッファは長さプレフィックス形式:
//   [u16 LE len][bytes][u16 LE len][bytes]...
// SYS_SPAWN の arg3=バッファポインタ, arg4=バッファ長で渡す。

use super::env::{CommandEnv, CommandEnvs};
pub use crate::ffi::OsString as EnvKey;
use crate::ffi::{OsStr, OsString};
use crate::num::NonZero;
use crate::os::sabos::ffi::OsStrExt;
use crate::path::Path;
use crate::process::StdioPipes;
use crate::sys::fs::File;
use crate::sys::pipe::Pipe;
use crate::{fmt, io};

/// 引数バッファの最大サイズ（バイト）
const ARGS_BUF_SIZE: usize = 4096;

////////////////////////////////////////////////////////////////////////////////
// syscall ヘルパー（インラインアセンブリ）
////////////////////////////////////////////////////////////////////////////////

/// SYS_SPAWN(31): プロセスをバックグラウンドで起動する。
///
/// 引数:
///   rdi — パスのポインタ
///   rsi — パスの長さ
///   rdx — 引数バッファのポインタ（0 なら引数なし）
///   r10 — 引数バッファの長さ
///
/// 戻り値:
///   正の値 — タスク ID
///   負の値 — エラー
fn syscall_spawn(path: &[u8], args_ptr: *const u8, args_len: usize) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 31u64,              // SYS_SPAWN
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            in("rdx") args_ptr as u64,
            in("r10") args_len as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_SPAWN_REDIRECTED(6): stdin/stdout リダイレクト付きでプロセスを起動する。
///
/// 引数:
///   rdi — SpawnRedirectArgs 構造体のポインタ（ユーザー空間）
///
/// 戻り値:
///   正の値 — タスク ID
///   負の値 — エラー
fn syscall_spawn_redirected(args: &SpawnRedirectArgs) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 6u64,               // SYS_SPAWN_REDIRECTED
            in("rdi") args as *const SpawnRedirectArgs as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_SPAWN_REDIRECTED に渡す引数構造体。
/// カーネル側の SpawnRedirectArgs と同じレイアウト（#[repr(C)]）。
#[repr(C)]
struct SpawnRedirectArgs {
    path_ptr: u64,
    path_len: u64,
    args_ptr: u64,
    args_len: u64,
    /// stdin のリダイレクト先ハンドル ID。u64::MAX = リダイレクトなし（コンソール）
    stdin_handle_id: u64,
    stdin_handle_token: u64,
    /// stdout のリダイレクト先ハンドル ID。u64::MAX = リダイレクトなし（コンソール）
    stdout_handle_id: u64,
    stdout_handle_token: u64,
}

/// SYS_WAIT(34): 子プロセスの終了を待つ。
///
/// 引数:
///   rdi — 待つタスク ID（0 なら任意の子）
///   rsi — タイムアウト（ms、0 なら無期限）
///
/// 戻り値:
///   0 以上 — 終了コード
///   負の値 — エラー
fn syscall_wait(task_id: u64, timeout_ms: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 34u64,              // SYS_WAIT
            in("rdi") task_id,
            in("rsi") timeout_ms,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_KILL(36): タスクを強制終了する。
///
/// 引数:
///   rdi — タスク ID
///
/// 戻り値:
///   0 — 成功
///   負の値 — エラー
fn syscall_kill(task_id: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 36u64,              // SYS_KILL
            in("rdi") task_id,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// 引数リストを SABOS 形式のバッファに変換する。
///
/// フォーマット: [u16 LE len][bytes][u16 LE len][bytes]...
/// args[0] はプログラム名なのでスキップし、args[1..] のみ書き込む。
fn build_args_buffer(args: &[OsString], buf: &mut [u8]) -> usize {
    let mut offset = 0;
    // args[0] はプログラム名（パスとして別途渡す）なのでスキップ
    for arg in args.iter().skip(1) {
        let bytes = arg.as_bytes();
        let len = bytes.len() as u16;
        let needed = 2 + bytes.len();
        if offset + needed > buf.len() {
            break;
        }
        let le_bytes = len.to_le_bytes();
        buf[offset] = le_bytes[0];
        buf[offset + 1] = le_bytes[1];
        offset += 2;
        buf[offset..offset + bytes.len()].copy_from_slice(bytes);
        offset += bytes.len();
    }
    offset
}

////////////////////////////////////////////////////////////////////////////////
// Command
////////////////////////////////////////////////////////////////////////////////

pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    env: CommandEnv,

    cwd: Option<OsString>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

// Stdio enum — MakePipe で stdin/stdout パイプリダイレクトをサポート
#[derive(Debug)]
pub enum Stdio {
    Inherit,
    Null,
    MakePipe,
    ParentStdout,
    ParentStderr,
    #[allow(dead_code)]
    InheritFile(File),
}

impl Command {
    pub fn new(program: &OsStr) -> Command {
        Command {
            program: program.to_owned(),
            args: vec![program.to_owned()],
            env: Default::default(),
            cwd: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    pub fn arg(&mut self, arg: &OsStr) {
        self.args.push(arg.to_owned());
    }

    pub fn env_mut(&mut self) -> &mut CommandEnv {
        &mut self.env
    }

    pub fn cwd(&mut self, dir: &OsStr) {
        self.cwd = Some(dir.to_owned());
    }

    pub fn stdin(&mut self, stdin: Stdio) {
        self.stdin = Some(stdin);
    }

    pub fn stdout(&mut self, stdout: Stdio) {
        self.stdout = Some(stdout);
    }

    pub fn stderr(&mut self, stderr: Stdio) {
        self.stderr = Some(stderr);
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn get_args(&self) -> CommandArgs<'_> {
        let mut iter = self.args.iter();
        iter.next();
        CommandArgs { iter }
    }

    pub fn get_envs(&self) -> CommandEnvs<'_> {
        self.env.iter()
    }

    pub fn get_env_clear(&self) -> bool {
        self.env.does_clear()
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.cwd.as_ref().map(|cs| Path::new(cs))
    }

    /// プロセスを起動する。
    ///
    /// stdin/stdout に MakePipe が指定されている場合は SYS_PIPE でパイプを作成し、
    /// SYS_SPAWN_REDIRECTED で子プロセスにパイプの一端を渡す。
    /// それ以外は SYS_SPAWN で通常起動する。
    pub fn spawn(
        &mut self,
        _default: Stdio,
        _needs_stdin: bool,
    ) -> io::Result<(Process, StdioPipes)> {
        // プログラムパスをバイト列に変換
        let program_bytes = self.program.as_bytes();

        // 引数バッファを構築（args[0] はプログラム名なのでスキップ）
        let mut args_buf = [0u8; ARGS_BUF_SIZE];
        let args_len = build_args_buffer(&self.args, &mut args_buf);

        let (args_ptr, args_buf_len) = if args_len > 0 {
            (args_buf.as_ptr(), args_len)
        } else {
            (core::ptr::null(), 0)
        };

        // パイプが必要かどうかを判定
        let needs_stdin_pipe = matches!(self.stdin, Some(Stdio::MakePipe));
        let needs_stdout_pipe = matches!(self.stdout, Some(Stdio::MakePipe));

        if needs_stdin_pipe || needs_stdout_pipe {
            // パイプ付きスポーン: SYS_SPAWN_REDIRECTED を使用
            self.spawn_with_pipes(program_bytes, args_ptr, args_buf_len, needs_stdin_pipe, needs_stdout_pipe)
        } else {
            // 通常スポーン: SYS_SPAWN を使用
            self.spawn_simple(program_bytes, args_ptr, args_buf_len)
        }
    }

    /// 通常のスポーン（パイプなし）。SYS_SPAWN を使用する。
    fn spawn_simple(
        &self,
        program_bytes: &[u8],
        args_ptr: *const u8,
        args_len: usize,
    ) -> io::Result<(Process, StdioPipes)> {
        let ret = syscall_spawn(program_bytes, args_ptr, args_len);

        if ret < 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "SYS_SPAWN failed: program not found or execution error",
            ));
        }

        let process = Process {
            task_id: ret as u64,
            status: None,
        };

        let pipes = StdioPipes {
            stdin: None,
            stdout: None,
            stderr: None,
        };

        Ok((process, pipes))
    }

    /// パイプ付きスポーン。SYS_PIPE + SYS_SPAWN_REDIRECTED を使用する。
    ///
    /// ## 動作
    ///
    /// stdout パイプの場合:
    /// 1. pipe() で (read_end, write_end) を作成
    /// 2. write_end を子プロセスの stdout として渡す
    /// 3. 親で write_end を drop（子プロセス終了時に EOF が発生するために必要）
    /// 4. 親で read_end を StdioPipes.stdout に返す
    ///
    /// stdin パイプの場合:
    /// 1. pipe() で (read_end, write_end) を作成
    /// 2. read_end を子プロセスの stdin として渡す
    /// 3. 親で read_end を drop
    /// 4. 親で write_end を StdioPipes.stdin に返す
    fn spawn_with_pipes(
        &self,
        program_bytes: &[u8],
        args_ptr: *const u8,
        args_len: usize,
        needs_stdin_pipe: bool,
        needs_stdout_pipe: bool,
    ) -> io::Result<(Process, StdioPipes)> {
        // stdin パイプの作成
        let (stdin_child_handle_id, stdin_child_handle_token, stdin_parent_pipe) =
            if needs_stdin_pipe {
                let (read_end, write_end) = crate::sys::pipe::pipe()?;
                // 子には read_end を渡し、親は write_end を保持する
                // read_end の handle_id/token を取得する。
                // sys_spawn_redirected がハンドルを複製するため、
                // 親の read_end は通常通り drop して close する。
                let child_id = read_end.handle_id();
                let child_token = read_end.handle_token();
                // read_end はスコープ終了で drop → close される。
                // 子プロセスは複製されたハンドルを使うので問題ない。
                (child_id, child_token, Some(write_end))
            } else {
                (u64::MAX, 0u64, None)
            };

        // stdout パイプの作成
        let (stdout_child_handle_id, stdout_child_handle_token, stdout_parent_pipe) =
            if needs_stdout_pipe {
                let (read_end, write_end) = crate::sys::pipe::pipe()?;
                // 子には write_end を渡し、親は read_end を保持する
                // sys_spawn_redirected がハンドルを複製するため、
                // 親の write_end は通常通り drop して close する。
                let child_id = write_end.handle_id();
                let child_token = write_end.handle_token();
                // write_end はスコープ終了で drop → close される。
                // 子プロセスは複製されたハンドルを使うので問題ない。
                (child_id, child_token, Some(read_end))
            } else {
                (u64::MAX, 0u64, None)
            };

        // SYS_SPAWN_REDIRECTED で子プロセスを起動
        let spawn_args = SpawnRedirectArgs {
            path_ptr: program_bytes.as_ptr() as u64,
            path_len: program_bytes.len() as u64,
            args_ptr: args_ptr as u64,
            args_len: args_len as u64,
            stdin_handle_id: stdin_child_handle_id,
            stdin_handle_token: stdin_child_handle_token,
            stdout_handle_id: stdout_child_handle_id,
            stdout_handle_token: stdout_child_handle_token,
        };

        let ret = syscall_spawn_redirected(&spawn_args);

        if ret < 0 {
            // スポーン失敗 — 親側のパイプも閉じる（drop で自動 close）
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "SYS_SPAWN_REDIRECTED failed: program not found or execution error",
            ));
        }

        let process = Process {
            task_id: ret as u64,
            status: None,
        };

        let pipes = StdioPipes {
            stdin: stdin_parent_pipe,
            stdout: stdout_parent_pipe,
            stderr: None, // SABOS は stderr リダイレクト未対応
        };

        Ok((process, pipes))
    }
}

/// output — Command を実行して終了ステータスと stdout/stderr を返す。
///
/// stdout を MakePipe で子プロセスに接続し、子の出力をキャプチャする。
/// stderr リダイレクトは未対応なので空の Vec を返す。
pub fn output(cmd: &mut Command) -> io::Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    // stdout をパイプに設定
    cmd.stdout(Stdio::MakePipe);

    let (mut process, pipes) = cmd.spawn(Stdio::Inherit, false)?;

    // stdout パイプから子プロセスの出力を読み取る
    let mut stdout_data = Vec::new();
    if let Some(stdout_pipe) = pipes.stdout {
        // 子プロセスが終了して write_end が閉じられると EOF になる
        let _ = stdout_pipe.read_to_end(&mut stdout_data)?;
    }

    // 子プロセスの終了を待つ
    let status = process.wait()?;

    // stderr は未対応なので空
    Ok((status, stdout_data, Vec::new()))
}

impl From<ChildPipe> for Stdio {
    fn from(_pipe: ChildPipe) -> Stdio {
        // パイプのチェイン（child1.stdout → child2.stdin）は未対応
        panic!("pipe chaining (child1.stdout -> child2.stdin) is not supported on SABOS");
    }
}

impl From<io::Stdout> for Stdio {
    fn from(_: io::Stdout) -> Stdio {
        Stdio::ParentStdout
    }
}

impl From<io::Stderr> for Stdio {
    fn from(_: io::Stderr) -> Stdio {
        Stdio::ParentStderr
    }
}

impl From<File> for Stdio {
    fn from(file: File) -> Stdio {
        Stdio::InheritFile(file)
    }
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            let mut debug_command = f.debug_struct("Command");
            debug_command.field("program", &self.program).field("args", &self.args);
            if !self.env.is_unchanged() {
                debug_command.field("env", &self.env);
            }

            if self.cwd.is_some() {
                debug_command.field("cwd", &self.cwd);
            }

            if self.stdin.is_some() {
                debug_command.field("stdin", &self.stdin);
            }
            if self.stdout.is_some() {
                debug_command.field("stdout", &self.stdout);
            }
            if self.stderr.is_some() {
                debug_command.field("stderr", &self.stderr);
            }

            debug_command.finish()
        } else {
            if let Some(ref cwd) = self.cwd {
                write!(f, "cd {cwd:?} && ")?;
            }
            if self.env.does_clear() {
                write!(f, "env -i ")?;
            } else {
                let mut any_removed = false;
                for (key, value_opt) in self.get_envs() {
                    if value_opt.is_none() {
                        if !any_removed {
                            write!(f, "env ")?;
                            any_removed = true;
                        }
                        write!(f, "-u {} ", key.to_string_lossy())?;
                    }
                }
            }
            for (key, value_opt) in self.get_envs() {
                if let Some(value) = value_opt {
                    write!(f, "{}={value:?} ", key.to_string_lossy())?;
                }
            }
            if self.program != self.args[0] {
                write!(f, "[{:?}] ", self.program)?;
            }
            write!(f, "{:?}", self.args[0])?;

            for arg in &self.args[1..] {
                write!(f, " {:?}", arg)?;
            }
            Ok(())
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
// ExitStatus
////////////////////////////////////////////////////////////////////////////////

/// プロセスの終了ステータス。
/// SABOS の SYS_WAIT が返す終了コード（i32）を保持する。
#[derive(PartialEq, Eq, Clone, Copy, Debug, Default)]
pub struct ExitStatus(i32);

impl ExitStatus {
    pub fn exit_ok(&self) -> Result<(), ExitStatusError> {
        if self.0 == 0 {
            Ok(())
        } else {
            Err(ExitStatusError(NonZero::new(self.0).unwrap()))
        }
    }

    pub fn code(&self) -> Option<i32> {
        Some(self.0)
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exit status: {}", self.0)
    }
}

////////////////////////////////////////////////////////////////////////////////
// ExitStatusError
////////////////////////////////////////////////////////////////////////////////

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExitStatusError(NonZero<i32>);

impl Into<ExitStatus> for ExitStatusError {
    fn into(self) -> ExitStatus {
        ExitStatus(self.0.get())
    }
}

impl ExitStatusError {
    pub fn code(self) -> Option<NonZero<i32>> {
        Some(self.0)
    }
}

////////////////////////////////////////////////////////////////////////////////
// ExitCode
////////////////////////////////////////////////////////////////////////////////

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitCode(u8);

impl ExitCode {
    pub const SUCCESS: ExitCode = ExitCode(0);
    pub const FAILURE: ExitCode = ExitCode(1);

    pub fn as_i32(&self) -> i32 {
        self.0 as i32
    }
}

impl From<u8> for ExitCode {
    fn from(code: u8) -> Self {
        Self(code)
    }
}

////////////////////////////////////////////////////////////////////////////////
// Process
////////////////////////////////////////////////////////////////////////////////

/// 起動中のプロセスを表す。
/// SABOS のタスク ID を保持し、wait/kill で操作する。
pub struct Process {
    task_id: u64,
    status: Option<ExitStatus>,
}

impl Process {
    pub fn id(&self) -> u32 {
        self.task_id as u32
    }

    pub fn kill(&mut self) -> io::Result<()> {
        let ret = syscall_kill(self.task_id);
        if ret < 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "SYS_KILL failed",
            ));
        }
        Ok(())
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        // タイムアウト 0 = 無期限待ち
        let ret = syscall_wait(self.task_id, 0);
        if ret < 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "SYS_WAIT failed",
            ));
        }
        let status = ExitStatus(ret as i32);
        self.status = Some(status);
        Ok(status)
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        // タイムアウト 1ms で即座にポーリング
        let ret = syscall_wait(self.task_id, 1);
        if ret < 0 {
            // まだ終了していない（タイムアウト）
            return Ok(None);
        }
        let status = ExitStatus(ret as i32);
        self.status = Some(status);
        Ok(Some(status))
    }
}

////////////////////////////////////////////////////////////////////////////////
// CommandArgs
////////////////////////////////////////////////////////////////////////////////

pub struct CommandArgs<'a> {
    iter: crate::slice::Iter<'a, OsString>,
}

impl<'a> Iterator for CommandArgs<'a> {
    type Item = &'a OsStr;
    fn next(&mut self) -> Option<&'a OsStr> {
        self.iter.next().map(|os| &**os)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl<'a> ExactSizeIterator for CommandArgs<'a> {
    fn len(&self) -> usize {
        self.iter.len()
    }
    fn is_empty(&self) -> bool {
        self.iter.is_empty()
    }
}

impl<'a> fmt::Debug for CommandArgs<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter.clone()).finish()
    }
}

////////////////////////////////////////////////////////////////////////////////
// ChildPipe / read_output
////////////////////////////////////////////////////////////////////////////////

/// 子プロセスのパイプ端点。crate::sys::pipe::Pipe を直接使用する。
pub type ChildPipe = Pipe;

/// パイプからの読み取り — stdout/stderr パイプからデータを読み取る。
///
/// stdout パイプから read_to_end で全データを読み取り、stdout バッファに格納する。
/// stderr は SABOS 未対応のため、err パイプは drop されるだけ。
pub fn read_output(
    stdout_pipe: ChildPipe,
    stdout: &mut Vec<u8>,
    _stderr_pipe: ChildPipe,
    _stderr: &mut Vec<u8>,
) -> io::Result<()> {
    let _ = stdout_pipe.read_to_end(stdout)?;
    Ok(())
}
