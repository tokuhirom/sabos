// framebuffer.rs — GOP フレームバッファへの描画機能
//
// GOP から取得したフレームバッファに直接ピクセルを書き込んで
// テキストや図形を描画する。BltOp は矩形の塗りつぶしには便利だけど、
// 1ピクセルずつ描くにはフレームバッファ直接アクセスのほうが速い。
//
// グローバルライター (WRITER) を提供して、kprint!/kprintln! マクロで
// カーネルのどこからでも（割り込みハンドラからも）画面に出力できるようにする。

use alloc::vec::Vec;
use core::fmt;
use font8x8::UnicodeFonts;
use spin::Mutex;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};

// =================================================================
// グローバルフレームバッファライター
// =================================================================
//
// 割り込みハンドラ（キーボード等）から画面に文字を表示するには、
// FramebufferWriter がグローバルにアクセス可能でなければならない。
// spin::Mutex で排他制御し、Option で「まだ初期化されていない」状態を表す。

/// グローバルフレームバッファライター。
/// spin::Mutex で割り込みハンドラからの同時アクセスを排他制御する。
/// 初期化前は None。init_global_writer() で初期化する。
pub static WRITER: Mutex<Option<FramebufferWriter>> = Mutex::new(None);

/// グローバルフレームバッファライターを初期化する。
/// Exit Boot Services 後、フレームバッファ情報が確定してから呼ぶ。
pub fn init_global_writer(info: FramebufferInfo) {
    let mut writer = FramebufferWriter::from_info(info);
    writer.clear();
    writer.flush_dirty(); // ダーティ領域（= 全画面）を MMIO に転送して画面に反映
    *WRITER.lock() = Some(writer);
}

/// グローバルライターの前景色と背景色を設定する。
/// spin::Mutex で排他制御する。
pub fn set_global_colors(fg: (u8, u8, u8), bg: (u8, u8, u8)) {
    if let Some(writer) = WRITER.lock().as_mut() {
        writer.set_colors(fg, bg);
    }
}

/// グローバルフレームバッファの画面をクリアする。
/// シェルの clear コマンドで使う。
pub fn clear_global_screen() {
    if let Some(writer) = WRITER.lock().as_mut() {
        writer.clear();
        writer.flush_dirty();
    }
}

/// グローバルフレームバッファのサイズを取得する。
/// GUI のレイアウト計算などで使う。
pub fn screen_size() -> Option<(usize, usize)> {
    WRITER
        .lock()
        .as_ref()
        .map(|writer| (writer.width, writer.height))
}

/// ユーザー空間に渡すためのフレームバッファ情報。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfoSmall {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
    pub bytes_per_pixel: u32,
}

/// グローバルフレームバッファ情報を取得する。
pub fn screen_info() -> Option<FramebufferInfoSmall> {
    WRITER.lock().as_ref().map(|writer| FramebufferInfoSmall {
        width: writer.width as u32,
        height: writer.height as u32,
        stride: writer.stride as u32,
        pixel_format: pixel_format_to_u32(writer.pixel_format),
        bytes_per_pixel: 4,
    })
}

/// PixelFormat をユーザー空間向けの数値に変換する。
fn pixel_format_to_u32(format: PixelFormat) -> u32 {
    match format {
        PixelFormat::Rgb => 1,
        PixelFormat::Bgr => 2,
        PixelFormat::Bitmask => 3,
        PixelFormat::BltOnly => 4,
    }
}

/// ダーティ矩形。変更された領域の境界ボックスを追跡する。
///
/// 描画操作がバックバッファのどの領域を変更したかを記録し、
/// flush 時にその領域だけを MMIO に転送することで転送量を最小化する。
/// 座標は (x_min, y_min) が左上、(x_max, y_max) が右下（exclusive）。
#[derive(Debug, Clone, Copy)]
struct DirtyRect {
    x_min: usize,
    y_min: usize,
    x_max: usize, // exclusive
    y_max: usize, // exclusive
}

/// 描画エラー。
/// ユーザー空間からの引数ミスを検出するために使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawError {
    /// フレームバッファが初期化されていない
    NotInitialized,
    /// 座標やサイズが画面外
    OutOfBounds,
    /// サイズが 0
    InvalidSize,
}

/// 1 ピクセルを描画する（グローバル）。
pub fn draw_pixel_global(x: usize, y: usize, r: u8, g: u8, b: u8) -> Result<(), DrawError> {
    let mut guard = WRITER.lock();
    let Some(writer) = guard.as_mut() else {
        return Err(DrawError::NotInitialized);
    };

    if x >= writer.width || y >= writer.height {
        return Err(DrawError::OutOfBounds);
    }

    writer.put_pixel(x, y, r, g, b);
    writer.mark_dirty(x, y, 1, 1);
    writer.flush_dirty();
    Ok(())
}

/// 矩形を塗りつぶして描画する（グローバル）。
pub fn draw_rect_global(
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    r: u8,
    g: u8,
    b: u8,
) -> Result<(), DrawError> {
    let mut guard = WRITER.lock();
    let Some(writer) = guard.as_mut() else {
        return Err(DrawError::NotInitialized);
    };

    if w == 0 || h == 0 {
        return Err(DrawError::InvalidSize);
    }
    if x >= writer.width || y >= writer.height {
        return Err(DrawError::OutOfBounds);
    }
    let end_x = x.checked_add(w).ok_or(DrawError::OutOfBounds)?;
    let end_y = y.checked_add(h).ok_or(DrawError::OutOfBounds)?;
    if end_x > writer.width || end_y > writer.height {
        return Err(DrawError::OutOfBounds);
    }

    // バックバッファに矩形を描画してから、その領域だけ MMIO に転送。
    // 行テンプレートを作って行ごとに copy_within することで put_pixel ループより高速。
    let pixel = writer.make_pixel(r, g, b);
    let bpp = 4;
    let row_bytes = w * bpp;

    // 最初の行をテンプレートとして作成
    let first_row_offset = (y * writer.stride + x) * bpp;
    for xx in 0..w {
        let offset = first_row_offset + xx * bpp;
        writer.backbuf[offset..offset + bpp].copy_from_slice(&pixel);
    }

    // 残りの行は最初の行を copy_within でコピー（同じ色パターンなので行コピーで済む）
    for yy in (y + 1)..end_y {
        let dst = (yy * writer.stride + x) * bpp;
        writer.backbuf.copy_within(first_row_offset..first_row_offset + row_bytes, dst);
    }
    writer.mark_dirty(x, y, w, h);
    writer.flush_dirty();
    Ok(())
}

/// 直線を描画する（グローバル）。
pub fn draw_line_global(
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    r: u8,
    g: u8,
    b: u8,
) -> Result<(), DrawError> {
    let mut guard = WRITER.lock();
    let Some(writer) = guard.as_mut() else {
        return Err(DrawError::NotInitialized);
    };

    if x0 >= writer.width || y0 >= writer.height || x1 >= writer.width || y1 >= writer.height {
        return Err(DrawError::OutOfBounds);
    }

    // 直線の bounding box を事前計算してダーティ領域に使う。
    // 以前は全画面 flush していたが、dirty rect で bounding box だけ転送する。
    let bb_x = x0.min(x1);
    let bb_y = y0.min(y1);
    let bb_w = x0.max(x1) - bb_x + 1;
    let bb_h = y0.max(y1) - bb_y + 1;

    // Bresenham
    let mut x0 = x0 as i32;
    let mut y0 = y0 as i32;
    let x1 = x1 as i32;
    let y1 = y1 as i32;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;

    loop {
        if x0 >= 0 && y0 >= 0 {
            writer.put_pixel(x0 as usize, y0 as usize, r, g, b);
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = err * 2;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }

    // bounding box だけをダーティにして転送（以前の全画面 flush より高速）
    writer.mark_dirty(bb_x, bb_y, bb_w, bb_h);
    writer.flush_dirty();
    Ok(())
}

/// バッファの内容を矩形として描画する（グローバル）。
///
/// buf は RGBX（4 bytes/pixel）を想定。alpha は無視する。
pub fn draw_blit_global(
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    buf: &[u8],
) -> Result<(), DrawError> {
    let mut guard = WRITER.lock();
    let Some(writer) = guard.as_mut() else {
        return Err(DrawError::NotInitialized);
    };

    if w == 0 || h == 0 {
        return Err(DrawError::InvalidSize);
    }
    if x >= writer.width || y >= writer.height {
        return Err(DrawError::OutOfBounds);
    }
    let end_x = x.checked_add(w).ok_or(DrawError::OutOfBounds)?;
    let end_y = y.checked_add(h).ok_or(DrawError::OutOfBounds)?;
    if end_x > writer.width || end_y > writer.height {
        return Err(DrawError::OutOfBounds);
    }

    let pixel_count = w.checked_mul(h).ok_or(DrawError::OutOfBounds)?;
    let byte_len = pixel_count.checked_mul(4).ok_or(DrawError::OutOfBounds)?;
    if buf.len() < byte_len {
        return Err(DrawError::InvalidSize);
    }

    // ユーザー空間のバッファはネイティブピクセルフォーマット（BGR/RGB）で
    // 書き込み済みなので、ピクセル単位のフォーマット変換は不要。
    // 行単位の memcpy でバックバッファにコピーする。
    // これにより 78万回のピクセル変換 → 768回の行コピーに削減される。
    let row_bytes = w * 4;
    let mut src_offset = 0;
    for yy in y..end_y {
        let dst_offset = (yy * writer.stride + x) * 4;
        writer.backbuf[dst_offset..dst_offset + row_bytes]
            .copy_from_slice(&buf[src_offset..src_offset + row_bytes]);
        src_offset += row_bytes;
    }

    // 変更された矩形領域をダーティとしてマークし、MMIO に転送
    writer.mark_dirty(x, y, w, h);
    writer.flush_dirty();
    Ok(())
}

/// 指定位置に文字列を描画する（グローバル）。
pub fn draw_text_global(
    x: usize,
    y: usize,
    fg: (u8, u8, u8),
    bg: (u8, u8, u8),
    text: &str,
) -> Result<(), DrawError> {
    let mut guard = WRITER.lock();
    let Some(writer) = guard.as_mut() else {
        return Err(DrawError::NotInitialized);
    };

    if x >= writer.width || y >= writer.height {
        return Err(DrawError::OutOfBounds);
    }

    let old_fg = writer.fg_color;
    let old_bg = writer.bg_color;
    let old_x = writer.cursor_x;
    let old_y = writer.cursor_y;

    writer.set_colors(fg, bg);
    writer.cursor_x = x;
    writer.cursor_y = y;

    let _ = writer.write_str(text);

    writer.set_colors(old_fg, old_bg);
    writer.cursor_x = old_x;
    writer.cursor_y = old_y;

    // 各 draw_char が mark_dirty しているので、まとめて flush_dirty で転送。
    // 以前の全画面 flush() より効率的（テキスト領域の bounding box だけ転送）。
    writer.flush_dirty();
    Ok(())
}

/// kprint!/kprintln! マクロの内部実装。
/// フレームバッファとシリアルの両方に出力する。
/// spin::Mutex で排他制御する。
///
/// 割り込みハンドラ（キーボード、タイマー）は WRITER / SERIAL1 のロックを
/// 取得しないため、without_interrupts は不要。
/// without_interrupts を使うと、他タスクがロック保持中にスピン待ちになった際に
/// タイマー割り込みが発火せずデッドロックする問題があった。
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    // フレームバッファに出力
    if let Some(writer) = WRITER.lock().as_mut() {
        writer.write_fmt(args).unwrap();

        // draw_char / scroll_up が mark_dirty しているので、
        // flush_dirty で変更領域の bounding box だけ MMIO に転送する。
        // 以前は手動でスクロール判定して flush/flush_rect を切り替えていたが、
        // dirty rect トラッキングにより自動的に最小限の領域が転送される。
        writer.flush_dirty();
    }
    // シリアルにも出力（デュアル出力）
    // Exit Boot Services 後のデバッグに便利。
    // make run のターミナルにカーネルログが表示される。
    crate::serial::SERIAL1
        .lock()
        .write_fmt(args)
        .ok();
}

/// カーネル用 print! マクロ。グローバルフレームバッファに出力する。
/// 割り込みハンドラからも安全に呼べる。
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => ({
        $crate::framebuffer::_print(format_args!($($arg)*));
    });
}

/// カーネル用 println! マクロ。末尾に改行を付けて出力する。
#[macro_export]
macro_rules! kprintln {
    () => ($crate::kprint!("\n"));
    ($($arg:tt)*) => ($crate::kprint!("{}\n", format_args!($($arg)*)));
}

/// フレームバッファの情報を保持する構造体。
/// Exit Boot Services の前に GOP から情報を取得して保存しておく。
/// Exit 後は GOP が使えなくなるが、フレームバッファの物理アドレス自体は有効なまま残る。
#[derive(Clone, Copy)]
pub struct FramebufferInfo {
    pub fb_addr: u64,
    pub fb_size: usize,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub pixel_format: PixelFormat,
}

impl FramebufferInfo {
    /// GOP からフレームバッファ情報を取得する。
    /// Exit Boot Services の前に呼ぶこと。
    pub fn from_gop(gop: &mut GraphicsOutput) -> Self {
        let mode_info = gop.current_mode_info();
        let (width, height) = mode_info.resolution();
        let stride = mode_info.stride();
        let pixel_format = mode_info.pixel_format();
        let mut fb = gop.frame_buffer();
        let fb_addr = fb.as_mut_ptr() as u64;
        let fb_size = fb.size();

        Self { fb_addr, fb_size, width, height, stride, pixel_format }
    }
}

/// フレームバッファへの描画を担当する構造体。
///
/// GOP から取得したフレームバッファの生ポインタとメタ情報を持つ。
/// ピクセルフォーマット (RGB/BGR) やストライド（1行あたりのピクセル数）を
/// 考慮して正しい位置に書き込む。
pub struct FramebufferWriter {
    /// フレームバッファの先頭アドレス（MMIO）
    fb_ptr: *mut u8,
    /// フレームバッファのバイト数
    fb_size: usize,
    /// RAM 上のバックバッファ。
    /// すべての描画はまずここに書き込み、flush() で MMIO に一括転送する。
    /// MMIO への個別 write_volatile は 通常の RAM アクセスの 100〜1000 倍遅いため、
    /// バックバッファを経由することで描画性能が劇的に向上する。
    backbuf: Vec<u8>,
    /// 画面の幅（ピクセル）
    width: usize,
    /// 画面の高さ（ピクセル）
    height: usize,
    /// 1行あたりのピクセル数。width と同じとは限らない。
    /// GPU がアラインメントのためにパディングを入れることがある。
    stride: usize,
    /// ピクセルフォーマット (RGB or BGR)
    pixel_format: PixelFormat,
    /// テキスト描画用: 現在のカーソル X 位置（ピクセル）
    cursor_x: usize,
    /// テキスト描画用: 現在のカーソル Y 位置（ピクセル）
    cursor_y: usize,
    /// テキストの前景色 (R, G, B)
    fg_color: (u8, u8, u8),
    /// テキストの背景色 (R, G, B)
    bg_color: (u8, u8, u8),
    /// ダーティ矩形。描画操作で変更された領域を追跡する。
    /// flush_dirty() でこの領域だけを MMIO に転送し、None にリセットする。
    dirty: Option<DirtyRect>,
}

// FramebufferWriter は *mut u8（フレームバッファの生ポインタ）を持つため、
// コンパイラは自動で Send を実装しない。しかしフレームバッファは
// 単一の物理メモリ領域で、spin::Mutex で排他制御しているので安全。
unsafe impl Send for FramebufferWriter {}

/// font8x8 は 8x8 ピクセルのフォント。1文字あたり 8 バイト。
const CHAR_WIDTH: usize = 8;
const CHAR_HEIGHT: usize = 8;

impl FramebufferWriter {
    /// 保存済みの FramebufferInfo から FramebufferWriter を作成する。
    /// Exit Boot Services 後にフレームバッファを使い続けるために使う。
    pub fn from_info(info: FramebufferInfo) -> Self {
        // バックバッファを RAM 上に確保する。
        // fb_size 分のメモリをゼロ初期化して確保。
        // ヒープアロケータが init 済みである必要がある（main.rs の初期化順序で保証）。
        let backbuf = alloc::vec![0u8; info.fb_size];

        Self {
            fb_ptr: info.fb_addr as *mut u8,
            fb_size: info.fb_size,
            backbuf,
            width: info.width,
            height: info.height,
            stride: info.stride,
            pixel_format: info.pixel_format,
            cursor_x: 0,
            cursor_y: 0,
            fg_color: (255, 255, 255), // デフォルト白
            bg_color: (0, 0, 128),     // デフォルト紺
            dirty: None,
        }
    }

    /// 前景色と背景色を設定する。
    pub fn set_colors(&mut self, fg: (u8, u8, u8), bg: (u8, u8, u8)) {
        self.fg_color = fg;
        self.bg_color = bg;
    }

    /// 指定した矩形領域をダーティとしてマークする。
    ///
    /// 既存のダーティ矩形があれば、その境界ボックス（bounding box）を拡張する。
    /// なければ新しいダーティ矩形を作成する。
    /// 画面範囲外の座標は自動的にクリップされる。
    fn mark_dirty(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let new = DirtyRect {
            x_min: x,
            y_min: y,
            x_max: (x + w).min(self.width),
            y_max: (y + h).min(self.height),
        };
        self.dirty = Some(match self.dirty {
            Some(old) => DirtyRect {
                x_min: old.x_min.min(new.x_min),
                y_min: old.y_min.min(new.y_min),
                x_max: old.x_max.max(new.x_max),
                y_max: old.y_max.max(new.y_max),
            },
            None => new,
        });
    }

    /// ダーティ矩形の領域だけを MMIO フレームバッファに転送し、ダーティ状態をリセットする。
    ///
    /// ダーティ矩形がなければ何もしない（不要な MMIO 転送を完全に回避）。
    /// 複数の描画操作を行った後にまとめて呼ぶことで、
    /// 変更された領域の bounding box だけが 1 回の転送で済む。
    fn flush_dirty(&mut self) {
        if let Some(dirty) = self.dirty.take() {
            let w = dirty.x_max.saturating_sub(dirty.x_min);
            let h = dirty.y_max.saturating_sub(dirty.y_min);
            if w > 0 && h > 0 {
                self.flush_rect(dirty.x_min, dirty.y_min, w, h);
            }
        }
    }

    /// 画面全体を背景色で塗りつぶす。
    ///
    /// バックバッファ全体を背景色ピクセルで埋める。
    /// put_pixel を使わず chunks_exact_mut で直接埋めることで高速化。
    /// MMIO への転送は呼び出し元が flush() で行う。
    pub fn clear(&mut self) {
        let (r, g, b) = self.bg_color;
        let pixel = self.make_pixel(r, g, b);
        for chunk in self.backbuf.chunks_exact_mut(4) {
            chunk.copy_from_slice(&pixel);
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.mark_dirty(0, 0, self.width, self.height);
    }

    /// RGB 値をピクセルフォーマットに応じた 4 バイト配列に変換する。
    /// RGB フォーマットなら [R, G, B, 0]、BGR なら [B, G, R, 0]。
    #[inline(always)]
    fn make_pixel(&self, r: u8, g: u8, b: u8) -> [u8; 4] {
        match self.pixel_format {
            PixelFormat::Rgb => [r, g, b, 0],
            PixelFormat::Bgr => [b, g, r, 0],
            _ => [r, g, b, 0], // Bitmask 等は未対応、とりあえず RGB 扱い
        }
    }

    /// 指定座標に1ピクセルをバックバッファに書き込む。
    ///
    /// MMIO（フレームバッファ）には直接書き込まない。
    /// バックバッファ (RAM) に書き込むことで、MMIO の遅さを回避する。
    /// 実際の画面への反映は flush() または flush_rect() で行う。
    fn put_pixel(&mut self, x: usize, y: usize, r: u8, g: u8, b: u8) {
        if x >= self.width || y >= self.height {
            return;
        }

        // 1ピクセル = 4バイト (32bit)
        // オフセット = (y * stride + x) * 4
        let offset = (y * self.stride + x) * 4;
        if offset + 3 >= self.fb_size {
            return;
        }

        let pixel = self.make_pixel(r, g, b);
        // バックバッファ (RAM) に書き込む。
        // 通常のメモリアクセスなので MMIO の write_volatile より桁違いに速い。
        self.backbuf[offset..offset + 4].copy_from_slice(&pixel);
    }

    /// バックバッファの指定矩形領域だけを MMIO に転送する。
    ///
    /// 画面の一部だけが変更された場合、全画面 flush() より高速。
    /// 全幅かつ stride == width の場合は 1 回の連続コピーで転送する最適化つき。
    fn flush_rect(&self, x: usize, y: usize, w: usize, h: usize) {
        let bpp = 4usize;
        let end_y = (y + h).min(self.height);
        let actual_w = w.min(self.width.saturating_sub(x));

        // 全幅コピーかつ stride == width の場合: メモリが連続しているので 1 回の memcpy
        if x == 0 && actual_w == self.width && self.stride == self.width {
            let start = y * self.width * bpp;
            let len = ((end_y - y) * self.width * bpp).min(self.fb_size.saturating_sub(start));
            if len > 0 {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        self.backbuf.as_ptr().add(start),
                        self.fb_ptr.add(start),
                        len,
                    );
                }
            }
            return;
        }

        // 部分的な幅の場合: 行ごとにコピー
        let row_bytes = actual_w * bpp;
        for row in y..end_y {
            let start = (row * self.stride + x) * bpp;
            if start + row_bytes > self.fb_size {
                break;
            }
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.backbuf.as_ptr().add(start),
                    self.fb_ptr.add(start),
                    row_bytes,
                );
            }
        }
    }

    /// 指定座標に 8x8 の文字を1つ描画する。
    ///
    /// font8x8 のグリフデータは 8 バイトの配列で、
    /// 各バイトが 1 行分（8 ピクセル）のビットパターン。
    /// ビットが 1 なら前景色、0 なら背景色を描く。
    fn draw_char(&mut self, x: usize, y: usize, c: char) {
        // font8x8 からグリフを取得。未対応文字は '?' で代用。
        let glyph = font8x8::BASIC_FONTS
            .get(c)
            .unwrap_or_else(|| font8x8::BASIC_FONTS.get('?').unwrap());

        for (row, &bits) in glyph.iter().enumerate() {
            for col in 0..CHAR_WIDTH {
                // 各ビットをチェック。LSB が左端。
                let is_fg = (bits >> col) & 1 == 1;
                let (r, g, b) = if is_fg {
                    self.fg_color
                } else {
                    self.bg_color
                };
                self.put_pixel(x + col, y + row, r, g, b);
            }
        }
        // 文字 1 個分の矩形をダーティとしてマーク（put_pixel 64回分を 1 回で記録）
        self.mark_dirty(x, y, CHAR_WIDTH, CHAR_HEIGHT);
    }

    /// 文字列を現在のカーソル位置から描画する。
    /// 改行 ('\n') でカーソルを次の行に移す。
    /// 画面右端に達したら自動で折り返す。
    pub fn write_str(&mut self, s: &str) {
        for c in s.chars() {
            self.write_char(c);
        }
    }

    /// 画面を1行分（CHAR_HEIGHT ピクセル）上にスクロールする。
    ///
    /// バックバッファ (RAM) 上でスクロール処理を行う。
    /// copy_within は memmove 相当で、重なった領域も正しく処理する。
    /// RAM 上の操作なので、以前の MMIO 上での core::ptr::copy より桁違いに速い。
    /// MMIO への転送は呼び出し元が flush() で行う。
    fn scroll_up(&mut self) {
        let bytes_per_pixel: usize = 4;
        let row_bytes = self.stride * bytes_per_pixel;
        let scroll_bytes = CHAR_HEIGHT * row_bytes;
        let total_bytes = (self.height * row_bytes).min(self.backbuf.len());

        // バックバッファ内でデータを上にずらす（RAM 上の memmove、非常に高速）
        self.backbuf.copy_within(scroll_bytes..total_bytes, 0);

        // 最下行（スクロールで空いた部分）を背景色でクリア
        let (r, g, b) = self.bg_color;
        let pixel = self.make_pixel(r, g, b);
        let clear_start = total_bytes - scroll_bytes;
        for chunk in self.backbuf[clear_start..total_bytes].chunks_exact_mut(4) {
            chunk.copy_from_slice(&pixel);
        }

        // スクロールは画面全体に影響するので全画面をダーティにマーク
        self.mark_dirty(0, 0, self.width, self.height);
    }

    /// カーソルが画面下端を超えていたらスクロールする。
    /// 複数行分超えている場合も対応する。
    fn ensure_cursor_visible(&mut self) {
        while self.cursor_y + CHAR_HEIGHT > self.height {
            self.scroll_up();
            self.cursor_y -= CHAR_HEIGHT;
        }
    }

    /// カーソル位置の 1 文字分を背景色で塗りつぶす。
    /// バックスペースで文字を消すときに使う。
    fn erase_char_at_cursor(&mut self) {
        let (r, g, b) = self.bg_color;
        for row in 0..CHAR_HEIGHT {
            for col in 0..CHAR_WIDTH {
                self.put_pixel(self.cursor_x + col, self.cursor_y + row, r, g, b);
            }
        }
        self.mark_dirty(self.cursor_x, self.cursor_y, CHAR_WIDTH, CHAR_HEIGHT);
    }

    /// 1文字を現在のカーソル位置に描画し、カーソルを進める。
    fn write_char(&mut self, c: char) {
        match c {
            '\n' => {
                // 改行: X を先頭に戻し、Y を 1 行分下げる
                self.cursor_x = 0;
                self.cursor_y += CHAR_HEIGHT;
                self.ensure_cursor_visible();
            }
            '\r' => {
                // キャリッジリターン: X を先頭に戻す
                self.cursor_x = 0;
            }
            '\x08' => {
                // バックスペース: カーソルを1文字戻して、その位置を背景色で消す。
                // 行頭より前には戻らない（前の行への巻き戻しは未対応）。
                if self.cursor_x >= CHAR_WIDTH {
                    self.cursor_x -= CHAR_WIDTH;
                    self.erase_char_at_cursor();
                }
            }
            c => {
                // 画面右端に達したら折り返し
                if self.cursor_x + CHAR_WIDTH > self.width {
                    self.cursor_x = 0;
                    self.cursor_y += CHAR_HEIGHT;
                }

                // 画面下端を超えていたらスクロール
                self.ensure_cursor_visible();

                self.draw_char(self.cursor_x, self.cursor_y, c);
                self.cursor_x += CHAR_WIDTH;
            }
        }
    }
}

/// core::fmt::Write を実装して write!() マクロが使えるようにする。
/// これで write!(fb, "Hello {}!", name) のような書き方ができる。
impl fmt::Write for FramebufferWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_str(s);
        Ok(())
    }
}
