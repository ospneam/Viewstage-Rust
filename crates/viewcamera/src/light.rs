use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

// ============================================================
// USB HID Device Constants — Seewo (希沃) document camera
// ============================================================
const SEEWO_VID: u16 = 0x1FF7;
const SEEWO_PIDS: &[u16] = &[
    0x0F16, 0x0F18, 0x0F6E, 0x0F56, 0x0F59, 0x0F63, 0x0F6F, 0x0F81, 0x0F05, 0x0F51, 0x0F4E, 0x0F64,
];

// ============================================================
// USB HID Report Protocol
// ============================================================
const CMD_LIGHT_ON: [u8; 9] = [0xAA, 0xBB, 0xCC, 0x02, 0x02, 0x02, 0x00, 0x01, 0x32];
const CMD_LIGHT_OFF: [u8; 9] = [0xAA, 0xBB, 0xCC, 0x02, 0x02, 0x02, 0x00, 0x00, 0x00];
struct Monitor {
    running: Arc<AtomicBool>,
    pid: u16,
    _thread: thread::JoinHandle<()>,
}

static MONITOR: std::sync::LazyLock<Mutex<Option<Monitor>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

// ============================================================
// Device operations
// ============================================================

fn device_find_and_open() -> Result<(hidapi::HidDevice, u16), String> {
    let api = hidapi::HidApi::new().map_err(|e| format!("HID init failure: {}", e))?;
    for &pid in SEEWO_PIDS {
        if let Ok(device) = api.open(SEEWO_VID, pid) {
            return Ok((device, pid));
        }
    }
    Err("No supported camera found".to_string())
}

fn monitor_thread_loop(device: hidapi::HidDevice, running: Arc<AtomicBool>) {
    let mut buf = [0u8; 64];
    loop {
        if !running.load(Ordering::Relaxed) {
            break;
        }
        // 持续读取以维持设备连接与错误检测；上报内容无需处理（无状态查询方）
        match device.read(&mut buf) {
            Ok(_) => {}
            Err(_) => break,
        }
    }
    running.store(false, Ordering::Relaxed);
}

fn send_raw_command(cmd: &[u8]) -> Result<(), String> {
    let guard = MONITOR.lock().map_err(|e| format!("Lock failure: {}", e))?;
    let m = guard.as_ref().ok_or("Monitor not started")?;
    if !m.running.load(Ordering::Relaxed) {
        return Err("Monitor thread stopped".to_string());
    }
    let pid = m.pid;
    drop(guard);

    let api = hidapi::HidApi::new().map_err(|e| format!("HID init failure: {}", e))?;
    let device = api
        .open(SEEWO_VID, pid)
        .map_err(|e| format!("Open device for write failed: {}", e))?;
    device
        .write(cmd)
        .map_err(|e| format!("Write failed: {}", e))?;
    Ok(())
}

fn start_monitor() -> Result<(), String> {
    let mut guard = MONITOR.lock().map_err(|e| format!("Lock failure: {}", e))?;

    if let Some(ref m) = *guard {
        if m.running.load(Ordering::Relaxed) {
            return Ok(());
        }
        *guard = None;
    }

    let (device, pid) = device_find_and_open()?;
    let running = Arc::new(AtomicBool::new(true));

    let t_running = running.clone();
    let thread = thread::Builder::new()
        .name("camera-light-monitor".into())
        .spawn(move || monitor_thread_loop(device, t_running))
        .map_err(|e| format!("Thread spawn failed: {}", e))?;

    *guard = Some(Monitor {
        running,
        pid,
        _thread: thread,
    });
    drop(guard);
    Ok(())
}

// ============================================================
// Public API
// ============================================================

pub fn set_on() -> Result<(), String> {
    start_monitor()?;
    send_raw_command(&CMD_LIGHT_ON)
}

pub fn set_off() -> Result<(), String> {
    start_monitor()?;
    send_raw_command(&CMD_LIGHT_OFF)
}

pub fn toggle(is_on: bool) {
    thread::spawn(move || {
        if is_on {
            let _ = set_on();
        } else {
            let _ = set_off();
        }
    });
}
