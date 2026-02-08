// os/sabos/mod.rs — SABOS 固有の std 拡張
//
// OsStr/OsString のバイト列変換トレイトを提供する。
// Unix 系 OS と同じく、OsStr の内部表現はバイト列そのまま。

#![stable(feature = "rust1", since = "1.0.0")]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod ffi;

/// SABOS 固有のトレイトをまとめた prelude
#[stable(feature = "rust1", since = "1.0.0")]
pub mod prelude {
    #[doc(no_inline)]
    #[stable(feature = "rust1", since = "1.0.0")]
    pub use super::ffi::{OsStrExt, OsStringExt};
}
