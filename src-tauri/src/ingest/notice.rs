use tauri::{AppHandle, Manager};
use windows::Win32::System::Power::{
    ES_CONTINUOUS, ES_SYSTEM_REQUIRED, EXECUTION_STATE, SetThreadExecutionState,
};
use windows::Win32::{
    Foundation::HWND,
    UI::{
        Shell::*,
        WindowsAndMessaging::{IDI_INFORMATION, LoadIconW},
    },
};

/// A running USB transfer can finish while the monitor sleeps. Restore the
/// worker thread's previous request when the session exits, including on error.
pub(super) struct ActiveTransfer(EXECUTION_STATE);

impl ActiveTransfer {
    pub fn new() -> Self {
        Self(unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) })
    }
}

impl Drop for ActiveTransfer {
    fn drop(&mut self) {
        if self.0.0 != 0 {
            let _ = unsafe { SetThreadExecutionState(self.0 | ES_CONTINUOUS) };
        }
    }
}

fn wide<const N: usize>(s: &str) -> [u16; N] {
    let mut buffer = [0; N];
    for (slot, c) in buffer.iter_mut().take(N - 1).zip(s.encode_utf16()) {
        *slot = c;
    }
    buffer
}

pub(super) fn originals_safe(app: &AppHandle) {
    if let Some(tray) = app.tray_by_id("timecut") {
        let _ = tray.set_tooltip(Some("Time-cut"));
    }
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.request_user_attention(Some(tauri::UserAttentionType::Informational));
    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    let mut notice = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: HWND(hwnd.0),
        uID: 0x5443,
        uFlags: NIF_ICON | NIF_TIP,
        hIcon: unsafe { LoadIconW(None, IDI_INFORMATION) }.unwrap_or_default(),
        szTip: wide("Time-cut"),
        ..Default::default()
    };
    // Focus Assist may suppress this one-time notification; the current
    // transfer report remains available in the application.
    if unsafe { Shell_NotifyIconW(NIM_ADD, &notice) }.as_bool() {
        notice.uFlags = NIF_INFO;
        notice.dwInfoFlags = NIIF_INFO;
        notice.szInfoTitle = wide("Time-cut · 原件已收齐");
        notice.szInfo = wide("可以拔下手机。电脑会继续整理照片和视频。");
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &notice) };
        let handle = hwnd.0 as usize;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(40));
            let icon = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: HWND(handle as *mut _),
                uID: 0x5443,
                ..Default::default()
            };
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &icon) };
        });
    }
}
