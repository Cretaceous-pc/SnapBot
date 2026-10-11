//! 单实例锁。
//!
//! 为什么需要：用户可能双击两次。没有锁的话第二个实例会探测到端口已被监听
//! （那正是第一个实例起的网关）→ 判定为「附着」→ 开出第二个窗口。
//! 行为上不算错，但两个窗口共用同一网关，观感很差。
//!
//! 用命名互斥体而不是锁文件：进程崩溃时内核自动释放，不会留下需要手工清理的
//! 陈旧锁文件——锁文件的经典故障正是「崩溃后永远起不来」。

/// 尝试取得单实例所有权。
///
/// 返回 true 表示本进程是唯一实例，可以继续启动。
/// 返回 false 表示已有实例在运行，调用方应当直接退出。
#[cfg(windows)]
pub fn acquire() -> bool {
    use std::sync::OnceLock;

    // 句柄必须活到进程结束（不能让它析构，否则互斥体被释放、锁就没了）。
    static HANDLE: OnceLock<isize> = OnceLock::new();

    const ERROR_ALREADY_EXISTS: u32 = 183;
    const MUTEX_NAME: &str = "Local\\SnapBotDesktopSingleInstance";

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(
            attributes: *const u8,
            initial_owner: i32,
            name: *const u16,
        ) -> *mut u8;
        fn GetLastError() -> u32;
    }

    let name: Vec<u16> = MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if handle.is_null() {
            // 拿不到互斥体（极少见）：放行比把用户挡在外面更好
            crate::gateway::log_line("单实例互斥体创建失败，继续启动");
            return true;
        }
        let _ = HANDLE.set(handle as isize);
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return false;
        }
    }
    true
}

/// 非 Windows 平台暂不实现（本项目目标是 Windows 桌面分发）。
#[cfg(not(windows))]
pub fn acquire() -> bool {
    true
}

#[cfg(test)]
mod tests {
    /// 同一进程内重复调用不应误判为「已有实例」——
    /// 我们只在启动路径调用一次，这里主要防止把返回值写反。
    #[cfg(windows)]
    #[test]
    fn first_acquire_succeeds() {
        assert!(super::acquire());
    }
}
