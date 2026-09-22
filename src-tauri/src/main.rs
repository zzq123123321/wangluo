// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::synchapi::CreateMutexW;
use winapi::shared::winerror::ERROR_ALREADY_EXISTS;
use winapi::um::winuser::MessageBoxW;

const SINGLE_INSTANCE_MUTEX: &str = "Local\\com.localuser.cliplink.single-instance";

/// 返回 true 表示已有实例在运行（本进程应退出）。
fn already_running() -> bool {
    let name: Vec<u16> = OsStr::new(SINGLE_INSTANCE_MUTEX).encode_wide().chain(Some(0)).collect();
    unsafe {
        // 句柄故意不关闭：进程存活期间互斥体保持存在，进程退出时由系统回收。
        let _h = CreateMutexW(std::ptr::null_mut(), 1, name.as_ptr());
        GetLastError() == ERROR_ALREADY_EXISTS
    }
}

fn warn_already_running() {
    let text: Vec<u16> = OsStr::new("ClipLink 已在运行，请勿重复打开。").encode_wide().chain(Some(0)).collect();
    let title: Vec<u16> = OsStr::new("ClipLink").encode_wide().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), 0x00000040);
    }
}

fn main() {
    if already_running() {
        warn_already_running();
        return;
    }
    cliplink_lib::run()
}
