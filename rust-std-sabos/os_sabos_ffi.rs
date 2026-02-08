// os/sabos/ffi.rs — SABOS 固有の OsStr/OsString 拡張
//
// Unix 系と同じく、OsStr の内部表現をバイト列として扱う。
// Unix の os_str.rs を再利用する。

#![stable(feature = "rust1", since = "1.0.0")]

#[path = "../unix/ffi/os_str.rs"]
mod os_str;

#[stable(feature = "rust1", since = "1.0.0")]
pub use self::os_str::{OsStrExt, OsStringExt};
