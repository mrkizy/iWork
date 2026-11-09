//! 命令行参数（T03 第 7 步只解析，组合是 T04）。
//!
//! `--dev-trust` 只在 `dev` feature 下被认识：发布构建里这一段代码不存在，参数解析器不认识它，
//! 按未知参数拒绝启动（退出码 2），不绑定 socket（T03 方案 S3 第 ③ 步）。

use std::ffi::OsString;
use std::fmt;

use iwork_ipc::peer::TrustPolicy;

/// 解析好的参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonArgs {
    pub trust: TrustPolicy,
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

#[cfg(feature = "dev")]
const DEV_TRUST_FLAG: &str = "--dev-trust";

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<DaemonArgs, ArgError> {
    let mut out = DaemonArgs {
        trust: TrustPolicy::RequireSigner,
    };
    for arg in args {
        let arg = arg.into_string().map_err(ArgError::NotUtf8)?;
        match arg.as_str() {
            #[cfg(feature = "dev")]
            DEV_TRUST_FLAG => out.trust = TrustPolicy::DevTrust,
            _ => return Err(ArgError::Unknown(arg)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_args_requires_signer() {
        assert_eq!(parse(args(&[])).unwrap().trust, TrustPolicy::RequireSigner);
    }

    #[test]
    fn unknown_arg_is_rejected() {
        assert_eq!(
            parse(args(&["--foo"])),
            Err(ArgError::Unknown("--foo".into()))
        );
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
