// mouse.rs — PS/2 マウスドライバ（最小実装）
//
// - PS/2 コントローラ (i8042) を初期化してマウスを有効化
// - IRQ12 の割り込みで 3 バイトパケットを受信
// - カーソル位置とボタン状態を保持
//
// 今は「最小で動く」実装に寄せる。
// 高機能（ホイール/多ボタン等）は将来拡張。

use core::sync::atomic::{AtomicBool, Ordering};
use lazy_static::lazy_static;
use spin::Mutex;
use x86_64::instructions::port::Port;

const STATUS_PORT: u16 = 0x64;
const CMD_PORT: u16 = 0x64;
const DATA_PORT: u16 = 0x60;

// PS/2 コントローラコマンド
const CMD_ENABLE_AUX: u8 = 0xA8;
const CMD_READ_CONFIG: u8 = 0x20;
const CMD_WRITE_CONFIG: u8 = 0x60;
const CMD_WRITE_MOUSE: u8 = 0xD4;

// PS/2 マウスコマンド
const MOUSE_SET_DEFAULTS: u8 = 0xF6;
const MOUSE_ENABLE_STREAMING: u8 = 0xF4;

// ACK
const MOUSE_ACK: u8 = 0xFA;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MouseState {
    pub x: i32,
    pub y: i32,
    pub dx: i32,
    pub dy: i32,
    pub buttons: u8,
    pub _pad: [u8; 3],
}

impl MouseState {
    fn new() -> Self {
        Self {
            x: 0,
            y: 0,
            dx: 0,
            dy: 0,
            buttons: 0,
            _pad: [0; 3],
        }
    }
}

struct MouseInner {
    state: MouseState,
    updated: bool,
    screen_w: i32,
    screen_h: i32,
    packet: [u8; 3],
    packet_index: usize,
}

lazy_static! {
    static ref MOUSE: Mutex<MouseInner> = Mutex::new(MouseInner {
        state: MouseState::new(),
        updated: false,
        screen_w: 1,
        screen_h: 1,
        packet: [0; 3],
        packet_index: 0,
    });
}

static MOUSE_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// PS/2 マウスを初期化する。
/// 成功したら true。
pub fn init() -> bool {
    // 補助デバイス（マウス）を有効化
    write_command(CMD_ENABLE_AUX);

    // コントローラ設定バイトを読む
    write_command(CMD_READ_CONFIG);
    let mut config = read_data();

    // IRQ12 を有効化（bit 1）
    config |= 0x02;

    // 設定バイトを書き戻す
    write_command(CMD_WRITE_CONFIG);
    write_data(config);

    // マウスにコマンドを送信
    if !write_mouse_and_ack(MOUSE_SET_DEFAULTS) {
        return false;
    }
    if !write_mouse_and_ack(MOUSE_ENABLE_STREAMING) {
        return false;
    }

    // 画面サイズが分かっていれば中央に初期化する
    if let Some(info) = crate::framebuffer::screen_info() {
        set_screen_size(info.width as i32, info.height as i32);
    }

    MOUSE_INITIALIZED.store(true, Ordering::Relaxed);
    true
}

/// 初期化済みかどうか
pub fn is_initialized() -> bool {
    MOUSE_INITIALIZED.load(Ordering::Relaxed)
}

/// 画面サイズを設定する（カーソルのクランプ用）
pub fn set_screen_size(w: i32, h: i32) {
    let mut inner = MOUSE.lock();
    inner.screen_w = w.max(1);
    inner.screen_h = h.max(1);
    inner.state.x = inner.screen_w / 2;
    inner.state.y = inner.screen_h / 2;
    inner.updated = true;
}

/// IRQ12 ハンドラから呼ぶ。受信バイトを処理する。
pub fn handle_irq_byte(byte: u8) {
    let mut inner = MOUSE.lock();

    // 1 バイト目は bit3 が常に 1。崩れていたら同期を取り直す。
    if inner.packet_index == 0 && (byte & 0x08) == 0 {
        return;
    }

    let idx = inner.packet_index;
    inner.packet[idx] = byte;
    inner.packet_index = idx + 1;

    if inner.packet_index < 3 {
        return;
    }

    inner.packet_index = 0;
    let b0 = inner.packet[0];
    let b1 = inner.packet[1];
    let b2 = inner.packet[2];

    let dx = (b1 as i8) as i32;
    let dy = (b2 as i8) as i32;

    // y は上が正なので、画面座標 (下が +) に合わせて反転
    let dy = -dy;

    inner.state.dx = dx;
    inner.state.dy = dy;
    inner.state.buttons = b0 & 0x07;

    // 座標更新（画面内にクランプ）
    let mut x = inner.state.x + dx;
    let mut y = inner.state.y + dy;
    x = x.clamp(0, inner.screen_w - 1);
    y = y.clamp(0, inner.screen_h - 1);
    inner.state.x = x;
    inner.state.y = y;

    inner.updated = true;
}

/// マウス状態を取得する。
/// 変化が無ければ None を返す。
pub fn read_state() -> Option<MouseState> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut inner = MOUSE.lock();
        if !inner.updated {
            return None;
        }
        inner.updated = false;
        let state = inner.state;
        inner.state.dx = 0;
        inner.state.dy = 0;
        Some(state)
    })
}

fn write_command(cmd: u8) {
    wait_input_clear();
    unsafe { Port::<u8>::new(CMD_PORT).write(cmd) };
}

fn write_data(data: u8) {
    wait_input_clear();
    unsafe { Port::<u8>::new(DATA_PORT).write(data) };
}

fn read_data() -> u8 {
    wait_output_full();
    unsafe { Port::<u8>::new(DATA_PORT).read() }
}

fn write_mouse_and_ack(cmd: u8) -> bool {
    // マウス宛の書き込みは 0xD4 を経由する
    write_command(CMD_WRITE_MOUSE);
    write_data(cmd);
    let ack = read_data();
    ack == MOUSE_ACK
}

fn wait_input_clear() {
    // input buffer が空になるまで待つ（bit1 = 0）
    loop {
        let status = unsafe { Port::<u8>::new(STATUS_PORT).read() };
        if status & 0x02 == 0 {
            break;
        }
    }
}

fn wait_output_full() {
    // output buffer にデータが来るまで待つ（bit0 = 1）
    loop {
        let status = unsafe { Port::<u8>::new(STATUS_PORT).read() };
        if status & 0x01 != 0 {
            break;
        }
    }
}
