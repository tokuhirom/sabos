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

/// kprint!/kprintln! マクロの内部実装。
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
        if let Some(writer) = WRITER.lock().as_mut() {
            writer.write_fmt(args).unwrap();
        }
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
    /// GOP から FramebufferWriter を作成する。
    ///
    /// GOP のフレームバッファに直接アクセスするため、
    /// この関数で取得したポインタが有効な間だけ使える。
    pub fn new(gop: &mut GraphicsOutput) -> Self {
        let info = FramebufferInfo::from_gop(gop);
        Self::from_info(info)
    }

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
