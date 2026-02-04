// gui_client.rs — GUI IPC クライアント（共通部品）
//
// GUI サービスに対する描画要求をまとめた API。
// IPC のヘッダ生成やレスポンス検証を隠蔽する。

#![allow(dead_code)]

use crate::json;
use crate::syscall;

const GUI_TASK_NAME: &str = "GUI.ELF";

const OPCODE_CLEAR: u32 = 1;
const OPCODE_RECT: u32 = 2;
const OPCODE_LINE: u32 = 3;
const OPCODE_PRESENT: u32 = 4;
const OPCODE_CIRCLE: u32 = 5;

const IPC_REQ_HEADER: usize = 8;
const IPC_RESP_HEADER: usize = 12;

const IPC_BUF_SIZE: usize = 2048;

/// GUI クライアント
pub struct GuiClient {
    gui_id: u64,
}

impl GuiClient {
    /// 新しい GUI クライアントを作成
    pub const fn new() -> Self {
        Self { gui_id: 0 }
    }

    /// 画面クリア
    pub fn clear(&mut self, r: u8, g: u8, b: u8) -> Result<(), ()> {
        let payload = [r, g, b];
        let status = self.request(OPCODE_CLEAR, &payload)?;
        if status < 0 { Err(()) } else { Ok(()) }
    }

    /// 矩形描画
    pub fn rect(&mut self, x: u32, y: u32, w: u32, h: u32, r: u8, g: u8, b: u8) -> Result<(), ()> {
        let payload = build_rect_payload(x, y, w, h, r, g, b);
        let status = self.request(OPCODE_RECT, &payload)?;
        if status < 0 { Err(()) } else { Ok(()) }
    }

    /// 直線描画
    pub fn line(&mut self, x0: u32, y0: u32, x1: u32, y1: u32, r: u8, g: u8, b: u8) -> Result<(), ()> {
        let payload = build_line_payload(x0, y0, x1, y1, r, g, b);
        let status = self.request(OPCODE_LINE, &payload)?;
        if status < 0 { Err(()) } else { Ok(()) }
    }

    /// バックバッファを表示
    pub fn present(&mut self) -> Result<(), ()> {
        let status = self.request(OPCODE_PRESENT, &[])?;
        if status < 0 { Err(()) } else { Ok(()) }
    }

    /// 円（outline/filled）を描画
    ///
    /// filled = true なら塗りつぶし。
    pub fn circle(
        &mut self,
        cx: u32,
        cy: u32,
        r: u32,
        red: u8,
        green: u8,
        blue: u8,
        filled: bool,
    ) -> Result<(), ()> {
        let payload = build_circle_payload(cx, cy, r, red, green, blue, filled);
        let status = self.request(OPCODE_CIRCLE, &payload)?;
        if status < 0 { Err(()) } else { Ok(()) }
    }

    /// GUI サービスに IPC 送信してレスポンスを受け取る
    fn request(&mut self, opcode: u32, payload: &[u8]) -> Result<i32, ()> {
        let gui_id = self.ensure_gui_id()?;

        let mut req = [0u8; IPC_BUF_SIZE];
        if IPC_REQ_HEADER + payload.len() > req.len() {
            return Err(());
        }
        req[0..4].copy_from_slice(&opcode.to_le_bytes());
        req[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        req[8..8 + payload.len()].copy_from_slice(payload);

        if syscall::ipc_send(gui_id, &req[..8 + payload.len()]) < 0 {
            // PID 変更に備えて再解決して1回だけリトライ
            self.gui_id = 0;
            let gui_id = self.ensure_gui_id()?;
            if syscall::ipc_send(gui_id, &req[..8 + payload.len()]) < 0 {
                return Err(());
            }
        }

        let mut resp = [0u8; 128];
        let mut sender = 0u64;
        let n = syscall::ipc_recv(&mut sender, &mut resp, 5000);
        if n < 0 {
            return Err(());
        }
        let n = n as usize;
        if n < IPC_RESP_HEADER {
            return Err(());
        }

        let resp_opcode = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
        if resp_opcode != opcode {
            return Err(());
        }
        let status = i32::from_le_bytes([resp[4], resp[5], resp[6], resp[7]]);
        Ok(status)
    }

    /// GUI のタスク ID を確保する
    fn ensure_gui_id(&mut self) -> Result<u64, ()> {
        if self.gui_id != 0 {
            return Ok(self.gui_id);
        }
        let id = resolve_task_id_by_name(GUI_TASK_NAME).ok_or(())?;
        self.gui_id = id;
        Ok(id)
    }
}

fn build_rect_payload(x: u32, y: u32, w: u32, h: u32, r: u8, g: u8, b: u8) -> [u8; 19] {
    let mut payload = [0u8; 19];
    payload[0..4].copy_from_slice(&x.to_le_bytes());
    payload[4..8].copy_from_slice(&y.to_le_bytes());
    payload[8..12].copy_from_slice(&w.to_le_bytes());
    payload[12..16].copy_from_slice(&h.to_le_bytes());
    payload[16] = r;
    payload[17] = g;
    payload[18] = b;
    payload
}

fn build_line_payload(x0: u32, y0: u32, x1: u32, y1: u32, r: u8, g: u8, b: u8) -> [u8; 19] {
    let mut payload = [0u8; 19];
    payload[0..4].copy_from_slice(&x0.to_le_bytes());
    payload[4..8].copy_from_slice(&y0.to_le_bytes());
    payload[8..12].copy_from_slice(&x1.to_le_bytes());
    payload[12..16].copy_from_slice(&y1.to_le_bytes());
    payload[16] = r;
    payload[17] = g;
    payload[18] = b;
    payload
}

fn build_circle_payload(
    cx: u32,
    cy: u32,
    r: u32,
    red: u8,
    green: u8,
    blue: u8,
    filled: bool,
) -> [u8; 17] {
    let mut payload = [0u8; 17];
    payload[0..4].copy_from_slice(&cx.to_le_bytes());
    payload[4..8].copy_from_slice(&cy.to_le_bytes());
    payload[8..12].copy_from_slice(&r.to_le_bytes());
    payload[12] = red;
    payload[13] = green;
    payload[14] = blue;
    payload[15] = if filled { 1 } else { 0 };
    payload[16] = 0;
    payload
}

/// タスク一覧から指定名のタスク ID を探す
fn resolve_task_id_by_name(name: &str) -> Option<u64> {
    let mut buf = [0u8; 4096];
    let result = syscall::get_task_list(&mut buf);
    if result < 0 {
        return None;
    }
    let len = result as usize;
    let Ok(s) = core::str::from_utf8(&buf[..len]) else {
        return None;
    };

    let (tasks_start, tasks_end) = json::json_find_array_bounds(s, "tasks")?;
    let mut i = tasks_start;
    let bytes = s.as_bytes();
    while i < tasks_end {
        while i < tasks_end && bytes[i] != b'{' && bytes[i] != b']' {
            i += 1;
        }
        if i >= tasks_end || bytes[i] == b']' {
            break;
        }

        let obj_end = json::find_matching_brace(s, i)?;
        if obj_end > tasks_end {
            break;
        }

        let obj = &s[i + 1..obj_end];
        let id = json::json_find_u64(obj, "id");
        let task_name = json::json_find_str(obj, "name");
        if let (Some(id), Some(task_name)) = (id, task_name) {
            if task_name == name {
                return Some(id);
            }
        }
        i = obj_end + 1;
    }
    None
}
