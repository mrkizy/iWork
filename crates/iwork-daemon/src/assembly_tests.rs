use super::*;

fn probe(path: Option<&str>) -> RuntimeProbe {
    RuntimeProbe {
        cli_path: path.map(PathBuf::from),
        cli_version: path.map(|_| "2.1.322".into()),
        login: None,
    }
}

/// B1：桥接起的是探测到的那个 CLI（经 `IWORK_CLAUDE_CLI` 给绝对路径），不是 PATH 上找得到的那个；
/// 数据目录经 `IWORK_DATA_DIR` 给。探测没有路径就不组装。
#[test]
fn launch_uses_probed_cli_not_path() {
    let path_claude = "/usr/local/bin/claude";
    let probed = "/opt/probed/claude";
    let l = runtime_launch(
        PathBuf::from("/app/iwork-bridge-claude"),
        std::path::Path::new("/data"),
        &probe(Some(probed)),
    )
    .expect("探测到了就组装");
    assert_eq!(l.program, PathBuf::from("/app/iwork-bridge-claude"));
    let env: BTreeMap<_, _> = l.env.into_iter().collect();
    assert_eq!(env[&OsString::from(ENV_CLAUDE_CLI)], OsString::from(probed));
    assert_ne!(
        env[&OsString::from(ENV_CLAUDE_CLI)],
        OsString::from(path_claude)
    );
    assert_eq!(env[&OsString::from(ENV_DATA_DIR)], OsString::from("/data"));
    assert_eq!(env.len(), 2, "只给两个变量");
    assert!(
        runtime_launch(
            PathBuf::from("/b"),
            std::path::Path::new("/d"),
            &probe(None)
        )
        .is_none()
    );
}

/// 第 1 步：探测不到也有运行时那一行，状态 `Missing`、路径为空；编号是固定的那一个。
#[test]
fn runtime_row_is_missing_placeholder_when_not_found() {
    let row = runtime_row(&probe(None), "0.1.0");
    assert_eq!(row.id, RuntimeId::DEFAULT_RUNTIME);
    assert_eq!(row.state, RuntimeState::Missing);
    assert_eq!(row.cli_path, None);
    let found = runtime_row(&probe(Some("/opt/c")), "0.1.0");
    assert_eq!(found.state, RuntimeState::ReadyUnverified);
}

/// 桥接和后台程序同一个目录。
#[test]
fn bridge_sits_next_to_the_daemon() {
    let p = bridge_program(std::path::Path::new("/app/Contents/MacOS/iwork-daemon"));
    assert_eq!(
        p,
        PathBuf::from("/app/Contents/MacOS").join(crate::install::program_file(BRIDGE_NAME))
    );
}
