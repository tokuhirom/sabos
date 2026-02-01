// framebuffer.rs — GOP フレームバッファへの描画機能
//
// GOP から取得したフレームバッファに直接ピクセルを書き込んで
// テキストや図形を描画する。BltOp は矩形の塗りつぶしには便利だけど、
// 1ピクセルずつ描くにはフレームバッファ直接アクセスのほうが速い。

use core::fmt;
use font8x8::UnicodeFonts;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};

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

/// font8x8 は 8x8 ピクセルのフォント。1文字あたり 8 バイト。
const CHAR_WIDTH: usize = 8;
const CHAR_HEIGHT: usize = 8;

impl FramebufferWriter {
    /// GOP から FramebufferWriter を作成する。
    ///
    /// GOP のフレームバッファに直接アクセスするため、
    /// この関数で取得したポインタが有効な間だけ使える。
    pub fn new(gop: &mut GraphicsOutput) -> Self {
        let mode_info = gop.current_mode_info();
        let (width, height) = mode_info.resolution();
        let stride = mode_info.stride();
        let pixel_format = mode_info.pixel_format();

        // フレームバッファの生ポインタとサイズを取得
        let mut fb = gop.frame_buffer();
        let fb_ptr = fb.as_mut_ptr();
        let fb_size = fb.size();

        Self {
            fb_ptr,
            fb_size,
            width,
            height,
            stride,
            pixel_format,
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

    /// 1文字を現在のカーソル位置に描画し、カーソルを進める。
    fn write_char(&mut self, c: char) {
        match c {
            '\n' => {
                // 改行: X を先頭に戻し、Y を 1 行分下げる
                self.cursor_x = 0;
                self.cursor_y += CHAR_HEIGHT;
            }
            '\r' => {
                // キャリッジリターン: X を先頭に戻す
                self.cursor_x = 0;
            }
            c => {
                // 画面右端に達したら折り返し
                if self.cursor_x + CHAR_WIDTH > self.width {
                    self.cursor_x = 0;
                    self.cursor_y += CHAR_HEIGHT;
                }

                // 画面下端を超えたらスクロール...は未実装。
                // とりあえず描画を止める。
                if self.cursor_y + CHAR_HEIGHT > self.height {
                    return;
                }

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
