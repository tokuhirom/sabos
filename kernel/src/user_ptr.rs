// user_ptr.rs — ユーザー空間ポインタの型安全なラッパー
//
// SABOS の設計原則に従い、ユーザー空間から渡されるポインタを
// 型安全にラップする。これにより以下を実現:
//
// 1. null 終端文字列の排除 — すべて長さ付きスライス形式
// 2. アドレス範囲の検証 — ユーザー空間の有効な範囲かチェック
// 3. アラインメント検証 — 型 T が要求するアラインメントを満たすかチェック
//
// 使用例:
//   let user_slice = UserSlice::<u8>::from_raw(ptr, len)?;
//   let data = user_slice.read_to_vec()?;

use core::marker::PhantomData;

/// ユーザー空間アドレスの有効範囲
///
/// 現在の SABOS では全メモリが USER_ACCESSIBLE フラグ付きでマップされているため、
/// この範囲チェックは将来のセキュリティ強化のための準備。
///
/// 典型的な x86_64 のユーザー空間: 0x0000_0000_0000_0000 〜 0x0000_7FFF_FFFF_FFFF
/// カーネル空間: 0xFFFF_8000_0000_0000 〜 0xFFFF_FFFF_FFFF_FFFF
///
/// 現在は学習用として広い範囲を許可。
const USER_SPACE_START: u64 = 0x0000_0000_0000_0000;
const USER_SPACE_END: u64 = 0x0000_7FFF_FFFF_FFFF;

/// システムコールで発生しうるエラー
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyscallError {
    /// ポインタが null
    NullPointer,
    /// アドレスがユーザー空間の範囲外
    InvalidAddress,
    /// アラインメントが不正
    MisalignedPointer,
    /// バッファがユーザー空間をオーバーフロー
    BufferOverflow,
    /// 不正な UTF-8 文字列
    InvalidUtf8,
    /// ファイルが見つからない
    FileNotFound,
    /// 不明なシステムコール
    UnknownSyscall,
    /// その他のエラー
    Other,
}

impl SyscallError {
    /// エラーコードに変換（負の値として返す）
    /// ユーザー空間に返すときに使用
    pub fn to_errno(self) -> u64 {
        // Linux 風のエラーコード（負の値）
        // ただし u64 として返すので、符号拡張された形になる
        let code: i64 = match self {
            SyscallError::NullPointer => -1,
            SyscallError::InvalidAddress => -14,      // EFAULT
            SyscallError::MisalignedPointer => -22,   // EINVAL
            SyscallError::BufferOverflow => -14,      // EFAULT
            SyscallError::InvalidUtf8 => -22,         // EINVAL
            SyscallError::FileNotFound => -2,         // ENOENT
            SyscallError::UnknownSyscall => -38,      // ENOSYS
            SyscallError::Other => -1,                // EPERM
        };
        code as u64
    }
}

/// ユーザー空間の単一ポインタをラップする型
///
/// T はポイント先の型。検証済みのポインタのみをこの型で表現する。
///
/// # 使用例
/// ```ignore
/// // システムコールハンドラ内で
/// let user_ptr = UserPtr::<u32>::from_raw(arg1)?;
/// let value = user_ptr.read()?;
/// ```
#[derive(Debug, Clone, Copy)]
pub struct UserPtr<T> {
    /// 検証済みのユーザー空間アドレス
    addr: u64,
    /// T の型情報を保持（実際にはゼロサイズ）
    _marker: PhantomData<*const T>,
}

impl<T> UserPtr<T> {
    /// 生のアドレスから UserPtr を作成し、検証を行う
    ///
    /// # 検証内容
    /// - null でないこと
    /// - ユーザー空間の範囲内であること
    /// - T のアラインメント要件を満たすこと
    pub fn from_raw(addr: u64) -> Result<Self, SyscallError> {
        // null チェック
        if addr == 0 {
            return Err(SyscallError::NullPointer);
        }

        // ユーザー空間の範囲チェック
        if addr < USER_SPACE_START || addr > USER_SPACE_END {
            return Err(SyscallError::InvalidAddress);
        }

        // アラインメントチェック
        let align = core::mem::align_of::<T>() as u64;
        if addr % align != 0 {
            return Err(SyscallError::MisalignedPointer);
        }

        Ok(Self {
            addr,
            _marker: PhantomData,
        })
    }

    /// 検証済みアドレスを取得
    pub fn addr(&self) -> u64 {
        self.addr
    }

    /// ポインタとして取得（unsafe が必要な操作の準備）
    pub fn as_ptr(&self) -> *const T {
        self.addr as *const T
    }

    /// 可変ポインタとして取得
    pub fn as_mut_ptr(&self) -> *mut T {
        self.addr as *mut T
    }

    /// ユーザー空間から値を読み取る
    ///
    /// # Safety
    /// この関数自体は safe だが、内部で unsafe 操作を行う。
    /// UserPtr の作成時に検証が完了しているため、
    /// ここでの読み取りは安全とみなす。
    pub fn read(&self) -> T
    where
        T: Copy,
    {
        // UserPtr 作成時に検証済みなので、ここでの読み取りは安全
        unsafe { core::ptr::read(self.as_ptr()) }
    }

    /// ユーザー空間に値を書き込む
    pub fn write(&self, value: T)
    where
        T: Copy,
    {
        unsafe { core::ptr::write(self.as_mut_ptr(), value) }
    }
}

/// ユーザー空間のスライス（ポインタ + 長さ）をラップする型
///
/// SABOS の設計原則「null 終端文字列をカーネル API から排除する」を
/// 実現するための中核的な型。すべてのバッファは (ptr, len) 形式で渡す。
///
/// # 使用例
/// ```ignore
/// // SYS_WRITE のハンドラ内で
/// let user_slice = UserSlice::<u8>::from_raw(buf_ptr, buf_len)?;
/// let content = user_slice.as_slice();
/// ```
#[derive(Debug, Clone, Copy)]
pub struct UserSlice<T> {
    /// スライスの先頭アドレス
    addr: u64,
    /// 要素数（バイト数ではなく T の個数）
    len: usize,
    /// T の型情報を保持
    _marker: PhantomData<*const T>,
}

impl<T> UserSlice<T> {
    /// 生のアドレスと長さから UserSlice を作成し、検証を行う
    ///
    /// # 検証内容
    /// - 長さが 0 でなければポインタが null でないこと
    /// - 先頭アドレスがユーザー空間の範囲内であること
    /// - スライス全体がユーザー空間の範囲内であること（オーバーフローチェック）
    /// - T のアラインメント要件を満たすこと
    pub fn from_raw(addr: u64, len: usize) -> Result<Self, SyscallError> {
        // 長さ 0 の場合は特別扱い（空スライスは許可）
        if len == 0 {
            return Ok(Self {
                addr: 0,  // 長さ 0 なら addr は使わない
                len: 0,
                _marker: PhantomData,
            });
        }

        // null チェック（長さが 0 でない場合のみ）
        if addr == 0 {
            return Err(SyscallError::NullPointer);
        }

        // ユーザー空間の範囲チェック（先頭）
        if addr < USER_SPACE_START || addr > USER_SPACE_END {
            return Err(SyscallError::InvalidAddress);
        }

        // スライスの終端アドレスを計算（オーバーフローチェック付き）
        let size = core::mem::size_of::<T>();
        let total_size = size.checked_mul(len).ok_or(SyscallError::BufferOverflow)?;
        let end_addr = addr
            .checked_add(total_size as u64)
            .ok_or(SyscallError::BufferOverflow)?;

        // ユーザー空間の範囲チェック（終端）
        // end_addr は exclusive なので <= ではなく < でチェック
        if end_addr > USER_SPACE_END + 1 {
            return Err(SyscallError::BufferOverflow);
        }

        // アラインメントチェック
        let align = core::mem::align_of::<T>() as u64;
        if addr % align != 0 {
            return Err(SyscallError::MisalignedPointer);
        }

        Ok(Self {
            addr,
            len,
            _marker: PhantomData,
        })
    }

    /// 検証済みアドレスを取得
    pub fn addr(&self) -> u64 {
        self.addr
    }

    /// 要素数を取得
    pub fn len(&self) -> usize {
        self.len
    }

    /// スライスが空かどうか
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Rust のスライスとして取得
    ///
    /// UserSlice 作成時に検証済みなので、ここでのスライス作成は安全。
    /// ただし、ユーザー空間のメモリを直接参照するため、
    /// ユーザープログラムが同時にメモリを変更する可能性がある点には注意。
    /// （現在の SABOS はシングルタスクなので問題なし）
    pub fn as_slice(&self) -> &[T] {
        if self.len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(self.addr as *const T, self.len) }
        }
    }

    /// 可変スライスとして取得
    pub fn as_mut_slice(&self) -> &mut [T] {
        if self.len == 0 {
            &mut []
        } else {
            unsafe { core::slice::from_raw_parts_mut(self.addr as *mut T, self.len) }
        }
    }
}

impl UserSlice<u8> {
    /// バイトスライスを UTF-8 文字列として解釈
    ///
    /// 不正な UTF-8 の場合は Err(SyscallError::InvalidUtf8) を返す。
    pub fn as_str(&self) -> Result<&str, SyscallError> {
        let bytes = self.as_slice();
        core::str::from_utf8(bytes).map_err(|_| SyscallError::InvalidUtf8)
    }

    /// バイトスライスを UTF-8 文字列として解釈（エラー時は置換）
    ///
    /// 不正な UTF-8 の場合は "<invalid utf-8>" を返す。
    pub fn as_str_lossy(&self) -> &str {
        let bytes = self.as_slice();
        core::str::from_utf8(bytes).unwrap_or("<invalid utf-8>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_ptr_null_check() {
        let result = UserPtr::<u32>::from_raw(0);
        assert_eq!(result.unwrap_err(), SyscallError::NullPointer);
    }

    #[test]
    fn test_user_slice_empty() {
        let result = UserSlice::<u8>::from_raw(0, 0);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_user_slice_invalid_address() {
        // カーネル空間のアドレス
        let result = UserSlice::<u8>::from_raw(0xFFFF_8000_0000_0000, 10);
        assert_eq!(result.unwrap_err(), SyscallError::InvalidAddress);
    }
}
