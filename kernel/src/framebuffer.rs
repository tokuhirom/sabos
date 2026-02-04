// framebuffer.rs — GOP フレームバッファへの描画機能
//
// GOP から取得したフレームバッファに直接ピクセルを書き込んで
// テキストや図形を描画する。BltOp は矩形の塗りつぶしには便利だけど、
// 1ピクセルずつ描くにはフレームバッファ直接アクセスのほうが速い。
//
// グローバルライター (WRITER) を提供して、kprint!/kprintln! マクロで
// カーネルのどこからでも（割り込みハンドラからも）画面に出力できるようにする。

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
    *WRITER.lock() = Some(writer);
}

/// グローバルライターの前景色と背景色を設定する。
/// 割り込み無効区間で実行して、割り込みハンドラとのデッドロックを防ぐ。
pub fn set_global_colors(fg: (u8, u8, u8), bg: (u8, u8, u8)) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        if let Some(writer) = WRITER.lock().as_mut() {
            writer.set_colors(fg, bg);
        }
    });
}

/// グローバルフレームバッファの画面をクリアする。
/// シェルの clear コマンドで使う。
pub fn clear_global_screen() {
    x86_64::instructions::interrupts::without_interrupts(|| {
        if let Some(writer) = WRITER.lock().as_mut() {
            writer.clear();
        }
    });
}

/// グローバルフレームバッファのサイズを取得する。
/// GUI のレイアウト計算などで使う。
pub fn screen_size() -> Option<(usize, usize)> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        WRITER
            .lock()
            .as_ref()
            .map(|writer| (writer.width, writer.height))
    })
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
    x86_64::instructions::interrupts::without_interrupts(|| {
        WRITER.lock().as_ref().map(|writer| FramebufferInfoSmall {
            width: writer.width as u32,
            height: writer.height as u32,
            stride: writer.stride as u32,
            pixel_format: pixel_format_to_u32(writer.pixel_format),
            bytes_per_pixel: 4,
        })
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
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut guard = WRITER.lock();
        let Some(writer) = guard.as_mut() else {
            return Err(DrawError::NotInitialized);
        };

        if x >= writer.width || y >= writer.height {
            return Err(DrawError::OutOfBounds);
        }

        writer.put_pixel(x, y, r, g, b);
        Ok(())
    })
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
    x86_64::instructions::interrupts::without_interrupts(|| {
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

        for yy in y..end_y {
            for xx in x..end_x {
                writer.put_pixel(xx, yy, r, g, b);
            }
        }
        Ok(())
    })
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
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut guard = WRITER.lock();
        let Some(writer) = guard.as_mut() else {
            return Err(DrawError::NotInitialized);
        };

        if x0 >= writer.width || y0 >= writer.height || x1 >= writer.width || y1 >= writer.height {
            return Err(DrawError::OutOfBounds);
        }

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

        Ok(())
    })
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
    x86_64::instructions::interrupts::without_interrupts(|| {
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

        let mut idx = 0;
        for yy in y..end_y {
            for xx in x..end_x {
                let r = buf[idx];
                let g = buf[idx + 1];
                let b = buf[idx + 2];
                writer.put_pixel(xx, yy, r, g, b);
                idx += 4;
            }
        }

        Ok(())
    })
}

/// 指定位置に文字列を描画する（グローバル）。
pub fn draw_text_global(
    x: usize,
    y: usize,
    fg: (u8, u8, u8),
    bg: (u8, u8, u8),
    text: &str,
) -> Result<(), DrawError> {
    x86_64::instructions::interrupts::without_interrupts(|| {
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

        Ok(())
    })
}

/// kprint!/kprintln! マクロの内部実装。
/// フレームバッファとシリアルの両方に出力する。
/// 割り込み無効区間でライターにアクセスして、デッドロックを防ぐ。
///
/// デッドロックの仕組み:
///   1. メインコードが WRITER.lock() を取得して書き込み中
///   2. キーボード割り込みが発生
///   3. キーボードハンドラが WRITER.lock() を取ろうとする
///   4. ロックはメインコードが持っている → 永久待ち（デッドロック）
///
/// without_interrupts で割り込みを一時的に無効化すれば、
/// ロック保持中に割り込みが入ることはなくなる。
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    x86_64::instructions::interrupts::without_interrupts(|| {
        // フレームバッファに出力
        if let Some(writer) = WRITER.lock().as_mut() {
            writer.write_fmt(args).unwrap();
        }
        // シリアルにも出力（デュアル出力）
        // Exit Boot Services 後のデバッグに便利。
        // make run のターミナルにカーネルログが表示される。
        crate::serial::SERIAL1
            .lock()
            .write_fmt(args)
            .ok();
    });
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
    /// フレームバッファの先頭アドレス
    fb_ptr: *mut u8,
    /// フレームバッファのバイト数
    fb_size: usize,
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
        Self {
            fb_ptr: info.fb_addr as *mut u8,
            fb_size: info.fb_size,
            width: info.width,
            height: info.height,
            stride: info.stride,
            pixel_format: info.pixel_format,
            cursor_x: 0,
            cursor_y: 0,
            fg_color: (255, 255, 255), // デフォルト白
            bg_color: (0, 0, 128),     // デフォルト紺
        }
    }

    /// 前景色と背景色を設定する。
    pub fn set_colors(&mut self, fg: (u8, u8, u8), bg: (u8, u8, u8)) {
        self.fg_color = fg;
        self.bg_color = bg;
    }

    /// 画面全体を背景色で塗りつぶす。
    pub fn clear(&mut self) {
        let (r, g, b) = self.bg_color;
        for y in 0..self.height {
            for x in 0..self.width {
                self.put_pixel(x, y, r, g, b);
            }
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
    }

    /// 指定座標に1ピクセルを書き込む。
    ///
    /// ピクセルフォーマット (RGB/BGR) に応じてバイト順を変える。
    /// UEFI の GOP フレームバッファは 1 ピクセル 4 バイト（32bit）。
    /// stride はピクセル単位なので、バイトオフセットは stride * 4。
    fn put_pixel(&self, x: usize, y: usize, r: u8, g: u8, b: u8) {
        if x >= self.width || y >= self.height {
            return;
        }

        // 1ピクセル = 4バイト (32bit)
        // オフセット = (y * stride + x) * 4
        let offset = (y * self.stride + x) * 4;
        if offset + 3 >= self.fb_size {
            return;
        }

        // ピクセルフォーマットに応じてバイト順を変える。
        // RGB: [R, G, B, reserved]
        // BGR: [B, G, R, reserved]  ← QEMU + OVMF はこっちが多い
        let pixel: [u8; 4] = match self.pixel_format {
            PixelFormat::Rgb => [r, g, b, 0],
            PixelFormat::Bgr => [b, g, r, 0],
            _ => [r, g, b, 0], // Bitmask 等は未対応、とりあえず RGB 扱い
        };

        // volatile write でフレームバッファに書き込む。
        // volatile にしないとコンパイラが最適化で書き込みを消す可能性がある。
        unsafe {
            let ptr = self.fb_ptr.add(offset);
            core::ptr::write_volatile(ptr as *mut [u8; 4], pixel);
        }
    }

    /// 指定座標に 8x8 の文字を1つ描画する。
    ///
    /// font8x8 のグリフデータは 8 バイトの配列で、
    /// 各バイトが 1 行分（8 ピクセル）のビットパターン。
    /// ビットが 1 なら前景色、0 なら背景色を描く。
    fn draw_char(&self, x: usize, y: usize, c: char) {
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
    /// フレームバッファの内容を上方向にコピーして、最下行を背景色で埋める。
    ///
    /// フレームバッファは MMIO（メモリマップドI/O）なので、
    /// 通常の RAM よりアクセスが遅い。大量のピクセルをコピーするので
    /// 目に見えるほど遅くなる可能性がある。
    /// 将来的にはバックバッファ（RAM 上のコピー）を使って高速化できる。
    fn scroll_up(&mut self) {
        let bytes_per_pixel: usize = 4;
        let row_bytes = self.stride * bytes_per_pixel;
        let scroll_bytes = CHAR_HEIGHT * row_bytes;
        let total_bytes = self.height * row_bytes;

        // フレームバッファの内容を CHAR_HEIGHT 行分上にずらす。
        // core::ptr::copy は memmove 相当で、重なった領域も正しく処理する。
        unsafe {
            core::ptr::copy(
                self.fb_ptr.add(scroll_bytes),
                self.fb_ptr,
                total_bytes - scroll_bytes,
            );
        }

        // 最下行（スクロールで空いた部分）を背景色でクリア
        let (r, g, b) = self.bg_color;
        let clear_start_y = self.height - CHAR_HEIGHT;
        for y in clear_start_y..self.height {
            for x in 0..self.width {
                self.put_pixel(x, y, r, g, b);
            }
        }
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
    fn erase_char_at_cursor(&self) {
        let (r, g, b) = self.bg_color;
        for row in 0..CHAR_HEIGHT {
            for col in 0..CHAR_WIDTH {
                self.put_pixel(self.cursor_x + col, self.cursor_y + row, r, g, b);
            }
        }
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
