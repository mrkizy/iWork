//! 命名管道的安全描述符（方案 2.2 Windows 行、2.3）。只是字符串，非 Windows 也编译、跑单测。

/// 管道的 SDDL：DACL 里只有 `user_sid` 一条允许 ACE，权限 `GA`（GENERIC_ALL）。
///
/// `P` 让这份 DACL 不继承任何父对象的 ACE，所以没写进来的人（其他用户、Administrators、SYSTEM）都打不开管道。
/// 同用户的进程挡不住，那由 `judge_peer` 负责。
pub(crate) fn pipe_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;{user_sid})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sddl_grants_only_the_given_sid_full_access_without_inheritance() {
        assert_eq!(
            pipe_sddl("S-1-5-21-1-2-3-1001"),
            "D:P(A;;GA;;;S-1-5-21-1-2-3-1001)"
        );
        // 对照：换一个 SID，字符串跟着变，说明 SID 真的进了 ACE。
        assert_ne!(
            pipe_sddl("S-1-5-21-1-2-3-1002"),
            pipe_sddl("S-1-5-21-1-2-3-1001")
        );
    }
}
