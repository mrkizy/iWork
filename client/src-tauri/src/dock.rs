//! macOS 的 Dock 角标与跳动、退出菜单的 Option 替换项（T09 方案 2.8、Q15、Q16）。
//!
//! 窗口关着（进程留在 Dock）时也要能设角标、让图标跳，所以不经窗口，直接调 NSApp。AppKit 只能在主线程上调：
//! 这里的函数都要在主线程上执行（调用方经 `AppHandle::run_on_main_thread` 转过来），不在主线程时只记日志。
//! 其他平台没有 Dock：全是空操作。

#[cfg(target_os = "macos")]
mod imp {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSRequestUserAttentionType};
    use objc2_foundation::NSString;

    fn app() -> Option<objc2::rc::Retained<NSApplication>> {
        match MainThreadMarker::new() {
            Some(mtm) => Some(NSApplication::sharedApplication(mtm)),
            None => {
                tracing::error!(target: "iwork_client", "Dock 操作不在主线程上，没做");
                None
            }
        }
    }

    /// 角标文字；`None` 清掉。
    pub fn set_badge(label: Option<&str>) {
        let Some(app) = app() else { return };
        let label = label.map(NSString::from_str);
        app.dockTile().setBadgeLabel(label.as_deref());
    }

    /// 让图标跳一次（Q15：Informational）。应用在前台时系统不跳。
    pub fn request_attention() {
        let Some(app) = app() else { return };
        app.requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
    }

    /// 把应用菜单里标题为 `title` 的那一项设成上一项的 Option 替换项（按住 Option 才显示，Q16）。
    /// 返回找没找到。
    pub fn make_alternate(title: &str) -> bool {
        let Some(app) = app() else { return false };
        let Some(menu) = app.mainMenu() else {
            return false;
        };
        for i in 0..menu.numberOfItems() {
            let Some(sub) = menu.itemAtIndex(i).and_then(|m| m.submenu()) else {
                continue;
            };
            for j in 0..sub.numberOfItems() {
                if let Some(item) = sub.itemAtIndex(j)
                    && item.title().to_string() == title
                {
                    item.setAlternate(true);
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn set_badge(_label: Option<&str>) {}
    pub fn request_attention() {}
    pub fn make_alternate(_title: &str) -> bool {
        false
    }
}

pub use imp::{make_alternate, request_attention, set_badge};
