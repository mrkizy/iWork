//! 命令行参数。
//!
//! - `--service`：服务管理器（launchd / systemd）起的实例带它（T04 方案 2.2、2.12）。发布构建也认识。
//! - `--dev-trust`：只在 `dev` feature 下被认识：发布构建里这一段代码不存在，参数解析器不认识它，
//!   按未知参数拒绝启动，不绑定 socket（T03 方案 S3 第 ③ 步）。

use std::ffi::{OsStr, OsString};
use std::fmt;

use iwork_ipc::peer::TrustPolicy;

/// 解析好的参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonArgs {
    pub trust: TrustPolicy,
    /// 服务管理器起的托管实例。
    pub service: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgError {
    Unknown(String),
    NotUtf8(OsString),
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgError::Unknown(a) => write!(f, "未知参数：{a}"),
            ArgError::NotUtf8(a) => write!(f, "参数不是合法的 UTF-8：{a:?}"),
        }
    }
}

/// 托管实例的标志。plist 与 systemd 单元文件的模板里写的就是它。
pub const SERVICE_FLAG: &str = "--service";

#[cfg(feature = "dev")]
const DEV_TRUST_FLAG: &str = "--dev-trust";

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<DaemonArgs, ArgError> {
    let mut out = DaemonArgs {
        trust: TrustPolicy::RequireSigner,
        service: false,
    };
    for arg in args {
        let arg = arg.into_string().map_err(ArgError::NotUtf8)?;
        match arg.as_str() {
            SERVICE_FLAG => out.service = true,
            #[cfg(feature = "dev")]
            DEV_TRUST_FLAG => out.trust = TrustPolicy::DevTrust,
            _ => return Err(ArgError::Unknown(arg)),
        }
    }
    Ok(out)
}

/// 原始参数里有没有 `--service` 这个词。参数解析失败时还没有解析结果，「是不是托管实例」靠它定
/// （方案 2.2「`Usage` 没有例外」）：不依赖解析成败，只看这一个词。
pub fn is_service(raw: &[OsString]) -> bool {
    raw.iter()
        .any(|a| a.as_os_str() == OsStr::new(SERVICE_FLAG))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_args_requires_signer_and_is_not_service() {
        let a = parse(args(&[])).unwrap();
        assert_eq!(a.trust, TrustPolicy::RequireSigner);
        assert!(!a.service);
    }

    #[test]
    fn service_flag_is_known_in_every_build() {
        assert!(parse(args(&["--service"])).unwrap().service);
    }

    #[test]
    fn unknown_arg_is_rejected() {
        assert_eq!(
            parse(args(&["--foo"])),
            Err(ArgError::Unknown("--foo".into()))
        );
    }

    /// 解析失败与否，`is_service` 的答案只看有没有 `--service` 这个词。
    #[test]
    fn is_service_does_not_depend_on_parse() {
        let cases: [(&[&str], bool); 6] = [
            (&[], false),
            (&["--service"], true),
            (&["--service", "--bogus"], true),
            (&["--bogus", "--service"], true),
            (&["--bogus"], false),
            // 只认整个词，前缀、拼接都不算
            (&["--service=1", "--services"], false),
        ];
        for (raw, want) in cases {
            let raw = args(raw);
            assert_eq!(is_service(&raw), want, "{raw:?}");
            if want && raw.len() > 1 {
                assert!(parse(raw.clone()).is_err(), "对照：{raw:?} 解析应失败");
            }
        }
    }

    /// 对拍：plist 与 systemd 单元里给后台的参数，能被本程序解析，且是托管实例（方案 7.2 `packaged_args_parse`）。
    #[cfg(unix)]
    #[test]
    fn packaged_args_parse() {
        use iwork_host::autostart::{linux, macos};
        let program = std::path::Path::new("/Applications/iWork.app/Contents/MacOS/iwork-daemon");
        let from_plist = macos::plist_args(&macos::plist_text(program));
        let (exe, from_unit) = linux::exec_start(&linux::unit_text(program)).unwrap();
        assert_eq!(exe, program);
        for raw in [from_plist, from_unit] {
            assert!(!raw.is_empty());
            let raw: Vec<OsString> = raw.into_iter().map(OsString::from).collect();
            let parsed = parse(raw.clone()).unwrap_or_else(|e| panic!("{raw:?}：{e}"));
            assert!(parsed.service && is_service(&raw), "{raw:?}");
            assert_eq!(
                parsed.trust,
                TrustPolicy::RequireSigner,
                "发布配置不带开发开关"
            );
        }
    }

    /// 对拍：服务管理器的重启配置只表达「非零（或被信号结束）就再拉、零就不拉」，不列具体退出码——报零还是非零
    /// 只由 2.2 的表决定（`exit::process_status`）。plist 用 `Program` 不用 `BundleProgram`；单元不加
    /// `RestartForceExitStatus`（N4）。
    #[cfg(unix)]
    #[test]
    fn service_restart_config_matches_exit_table() {
        use iwork_host::autostart::{linux, macos};
        let program = std::path::Path::new("/x/iwork-daemon");
        let plist = macos::plist_text(program);
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>SuccessfulExit</key><false/>"));
        assert!(plist.contains("<key>Program</key>") && !plist.contains("BundleProgram"));
        assert!(
            !plist.contains("Crashed"),
            "不用 KeepAlive.Crashed：它不等于「被信号结束」"
        );
        let unit = linux::unit_text(program);
        assert!(unit.contains("Restart=on-failure"));
        for forbidden in [
            "RestartForceExitStatus",
            "RestartPreventExitStatus",
            "SuccessExitStatus",
        ] {
            assert!(!unit.contains(forbidden), "{forbidden}");
        }
    }

    #[cfg(not(feature = "dev"))]
    #[test]
    fn release_build_does_not_know_dev_trust() {
        let flag = ["--dev", "-trust"].concat();
        assert_eq!(parse(args(&[&flag])), Err(ArgError::Unknown(flag.clone())));
    }

    #[cfg(feature = "dev")]
    #[test]
    fn dev_build_parses_dev_trust() {
        assert_eq!(
            parse(args(&["--dev-trust"])).unwrap().trust,
            TrustPolicy::DevTrust
        );
    }
}
