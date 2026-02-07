// sys/pal/sabos/mod.rs — SABOS Platform Abstraction Layer
//
// SABOS カーネル上で動作するユーザープログラム向けの PAL 実装。
// unsupported PAL をベースに、SABOS 固有のシステムコール呼び出しを追加する。
// Hermit OS の PAL 実装を参考にしている。

#![deny(unsafe_op_in_unsafe_fn)]

pub mod os;

mod common;
pub use common::*;
