//! Bash 子命令归类用的登记表（T07 方案 3.4）。加一个程序 = 加一行，连同选项表一起审。
//!
//! 白名单是**允许清单**：只有登记过的选项才算安全形态，出现任何未登记的选项、位置参数个数或角色
//! 对不上，就不是安全形态（归执行）。宁可多归执行，也不把有副作用的命令当读放行。

/// 归类用的程序标识：去掉路径前缀（`/usr/bin/git` → `git`）。凭据匹配与动作分类都用它，两边对程序名的
/// 理解一致（审查 1/F-8）；命令目标里保留原文，指纹与记住规则仍区分带不带前缀。
pub fn program_id(literal: &str) -> &str {
    literal.rsplit('/').next().unwrap_or(literal)
}

/// 路径参数的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Read,
    Write,
    Delete,
}

use Role::{Delete as D, Read as R, Write as W};

/// 选项的取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value {
    /// 取一个非路径值（数字、格式、模式、属主…）
    Word,
    /// 取一个路径，按这些角色抽动作
    Path(&'static [Role]),
    /// 目的目录（`cp -t DIR`）：值记写，全部位置参数都是源
    TargetDir,
    /// 模式由选项给出（`grep -e PAT`）：位置参数全部是路径
    PatternWord,
    /// 模式文件（`grep -f FILE`）：值记读，位置参数全部是路径
    PatternFile,
    /// 不取值，但把全部位置参数改成写（`install -d`）
    AllWriteFlag,
    /// 不取值，目的明确不是目录（`cp -T`）：只检查目的本身，不展开「目的 / 源文件名」
    NoTargetDirFlag,
}

/// 位置参数的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operands {
    /// 每个位置参数都是这些角色
    All(&'static [Role]),
    /// 前 `skip` 个不是路径（模式、权限位），其余是这些角色
    After { skip: usize, roles: &'static [Role] },
    /// 最后一个是目的，其余是源；目的目录由 `-t` 给出时全部是源
    SourcesThenDest {
        source: &'static [Role],
        dest: &'static [Role],
    },
}

/// 保底角色（方案 3.4，实施审查 1/F-1、8/F-4）：程序带未登记选项、不是安全形态时，位置参数仍按这些
/// 角色产出路径动作。标准是「这个程序在**任何**选项组合下可能对位置操作数做的最重的事」，降级只能更严。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Floor {
    /// 每个位置参数都是这些角色
    All(&'static [Role]),
    /// 最后一个是目的（按目录目标语义展开），其余是源
    SourcesThenDest {
        source: &'static [Role],
        dest: &'static [Role],
    },
}

/// 一个白名单程序的参数规则。
#[derive(Debug, Clone, Copy)]
pub struct ArgSpec {
    pub program: &'static str,
    /// 不取值的短选项（逐字母）
    pub flags: &'static str,
    /// 取值的短选项
    pub short_values: &'static [(char, Value)],
    /// 不取值的长选项（不含 `--`）
    pub long_flags: &'static [&'static str],
    /// 取值的长选项（不含 `--`）
    pub long_values: &'static [(&'static str, Value)],
    pub operands: Operands,
    pub max_operands: Option<usize>,
    /// 不是安全形态时的保底角色
    pub floor: Floor,
}

const fn spec(program: &'static str, flags: &'static str, operands: Operands) -> ArgSpec {
    ArgSpec {
        program,
        flags,
        short_values: &[],
        long_flags: &[],
        long_values: &[],
        operands,
        max_operands: None,
        floor: floor_of(operands),
    }
}

/// 保底角色默认取安全形态下的角色：模式、权限位这类前置参数也按同样角色记（只会多抽）；
/// 没有路径角色的（`basename`、`tr`）按读。写文件只经选项值的程序（`sort -o`）由降级时叠加的
/// 兜底候选覆盖。例外（`uniq` 的第二个操作数是输出文件）在白名单表里单独写。
const fn floor_of(operands: Operands) -> Floor {
    match operands {
        Operands::All(&[]) => Floor::All(&[R]),
        Operands::All(roles) | Operands::After { roles, .. } => Floor::All(roles),
        Operands::SourcesThenDest { source, dest } => Floor::SourcesThenDest { source, dest },
    }
}

const READ: Operands = Operands::All(&[R]);
const WRITE: Operands = Operands::All(&[W]);
const DELETE: Operands = Operands::All(&[D]);

/// 白名单（方案 3.4 表）。`find`、`dd` 的参数结构特殊，单独处理（见 `bash.rs`）。
pub const SAFE_COMMANDS: &[ArgSpec] = &[
    spec("cat", "nbsvetAE", READ),
    ArgSpec {
        short_values: &[('n', Value::Word), ('c', Value::Word)],
        long_values: &[("lines", Value::Word), ("bytes", Value::Word)],
        ..spec("head", "qv", READ)
    },
    ArgSpec {
        short_values: &[('n', Value::Word), ('c', Value::Word)],
        long_values: &[("lines", Value::Word), ("bytes", Value::Word)],
        ..spec("tail", "qvr", READ)
    },
    spec("wc", "lwcmL", READ),
    ArgSpec {
        short_values: &[('f', Value::Word)],
        long_values: &[("format", Value::Word)],
        ..spec("stat", "LlnqrxF", READ)
    },
    spec("md5", "qr", READ),
    spec("md5sum", "bc", READ),
    ArgSpec {
        short_values: &[('a', Value::Word)],
        ..spec("shasum", "bcpt", READ)
    },
    spec("sha1sum", "bc", READ),
    spec("sha256sum", "bc", READ),
    ArgSpec {
        short_values: &[('A', Value::Word), ('t', Value::Word), ('N', Value::Word)],
        ..spec("od", "bcdovx", READ)
    },
    ArgSpec {
        short_values: &[('n', Value::Word)],
        ..spec("strings", "a", READ)
    },
    ArgSpec {
        max_operands: Some(2),
        ..spec("cmp", "lsb", READ)
    },
    ArgSpec {
        short_values: &[('U', Value::Word)],
        long_flags: &["brief", "color", "no-color"],
        max_operands: Some(2),
        ..spec("diff", "uqrNwbiBa", READ)
    },
    spec("ls", "laAhRtrS1dFGpiocmnsu", READ),
    ArgSpec {
        short_values: &[('d', Value::Word)],
        ..spec("du", "shcakmL", READ)
    },
    spec("readlink", "fenm", READ),
    spec("realpath", "eqsm", READ),
    spec("basename", "a", Operands::All(&[])),
    spec("dirname", "", Operands::All(&[])),
    ArgSpec {
        short_values: &[
            ('d', Value::Word),
            ('f', Value::Word),
            ('c', Value::Word),
            ('b', Value::Word),
        ],
        ..spec("cut", "s", READ)
    },
    spec("tr", "cdsu", Operands::All(&[])),
    spec("nl", "", READ),
    spec("tac", "", READ),
    spec("rev", "", READ),
    ArgSpec {
        short_values: &[('s', Value::Word)],
        ..spec("column", "t", READ)
    },
    ArgSpec {
        short_values: &[('w', Value::Word)],
        ..spec("fold", "bs", READ)
    },
    ArgSpec {
        short_values: &[('w', Value::Word)],
        ..spec("fmt", "", READ)
    },
    ArgSpec {
        short_values: &[('t', Value::Word)],
        ..spec("expand", "i", READ)
    },
    ArgSpec {
        short_values: &[('t', Value::Word)],
        ..spec("unexpand", "a", READ)
    },
    grep("grep"),
    grep("egrep"),
    grep("fgrep"),
    ArgSpec {
        short_values: &[
            ('e', Value::PatternWord),
            ('f', Value::PatternFile),
            ('A', Value::Word),
            ('B', Value::Word),
            ('C', Value::Word),
            ('g', Value::Word),
            ('t', Value::Word),
            ('m', Value::Word),
        ],
        long_flags: &["hidden", "no-ignore", "files-with-matches", "count", "json"],
        long_values: &[("glob", Value::Word), ("type", Value::Word)],
        ..spec(
            "rg",
            "ivnlLcwxFHhoqsSuz",
            Operands::After {
                skip: 1,
                roles: &[R],
            },
        )
    },
    ArgSpec {
        short_values: &[('k', Value::Word), ('t', Value::Word)],
        ..spec("sort", "nrufbsVh", READ)
    },
    ArgSpec {
        max_operands: Some(1),
        floor: Floor::All(&[R, W]),
        ..spec("uniq", "cdui", READ)
    },
    spec("comm", "123", READ),
    ArgSpec {
        short_values: &[('t', Value::Word), ('1', Value::Word), ('2', Value::Word)],
        ..spec("join", "", READ)
    },
    ArgSpec {
        short_values: &[('d', Value::Word)],
        ..spec("paste", "s", READ)
    },
    spec("touch", "am", WRITE),
    ArgSpec {
        short_values: &[('m', Value::Word)],
        ..spec("mkdir", "pv", WRITE)
    },
    ArgSpec {
        short_values: &[('s', Value::Word)],
        ..spec("truncate", "c", WRITE)
    },
    spec(
        "chmod",
        "Rv",
        Operands::After {
            skip: 1,
            roles: &[W],
        },
    ),
    spec(
        "chown",
        "Rvh",
        Operands::After {
            skip: 1,
            roles: &[W],
        },
    ),
    spec(
        "chgrp",
        "Rvh",
        Operands::After {
            skip: 1,
            roles: &[W],
        },
    ),
    spec("tee", "a", WRITE),
    ArgSpec {
        short_values: &[('t', Value::TargetDir), ('T', Value::NoTargetDirFlag)],
        long_flags: &["no-target-directory"],
        long_values: &[("target-directory", Value::TargetDir)],
        ..spec(
            "cp",
            "rRpanfv",
            Operands::SourcesThenDest {
                source: &[R],
                dest: &[W],
            },
        )
    },
    ArgSpec {
        short_values: &[
            ('t', Value::TargetDir),
            ('m', Value::Word),
            ('d', Value::AllWriteFlag),
        ],
        ..spec(
            "install",
            "v",
            Operands::SourcesThenDest {
                source: &[R],
                dest: &[W],
            },
        )
    },
    ArgSpec {
        short_values: &[('t', Value::TargetDir), ('T', Value::NoTargetDirFlag)],
        long_flags: &["no-target-directory"],
        ..spec(
            "mv",
            "fnv",
            Operands::SourcesThenDest {
                source: &[R, D],
                dest: &[W],
            },
        )
    },
    ArgSpec {
        short_values: &[('t', Value::TargetDir)],
        ..spec(
            "ln",
            "sfnv",
            Operands::SourcesThenDest {
                source: &[R],
                dest: &[W],
            },
        )
    },
    spec("rm", "rRfidv", DELETE),
    spec("rmdir", "pv", DELETE),
    spec("unlink", "", DELETE),
    ArgSpec {
        short_values: &[('n', Value::Word)],
        ..spec("shred", "uz", DELETE)
    },
];

const fn grep(program: &'static str) -> ArgSpec {
    ArgSpec {
        short_values: &[
            ('e', Value::PatternWord),
            ('f', Value::PatternFile),
            ('A', Value::Word),
            ('B', Value::Word),
            ('C', Value::Word),
            ('m', Value::Word),
        ],
        long_flags: &["color"],
        long_values: &[("include", Value::Word), ("exclude", Value::Word)],
        ..spec(
            program,
            "ivnrRlLcwxEFPHhoqsz",
            Operands::After {
                skip: 1,
                roles: &[R],
            },
        )
    }
}

pub fn safe_command(program: &str) -> Option<&'static ArgSpec> {
    SAFE_COMMANDS.iter().find(|s| s.program == program)
}

/// `find` 的前置选项（方案 3.4「`find` 按它自己的语法切」第 1 步）：不带值、可以合写的字母。
pub const FIND_LEADING_LETTERS: &str = "HLPEXdsx";
/// `find` 带一个值的前置选项（`-D <值>`）。`-f <路径>` 的值是起点，单独处理；`-O<级别>` 粘值。
pub const FIND_LEADING_VALUE: &[&str] = &["-D"];
/// `find` 表达式里取一个非路径值的谓词。
pub const FIND_VALUE_PREDICATES: &[&str] = &[
    "-name",
    "-iname",
    "-path",
    "-ipath",
    "-type",
    "-size",
    "-mtime",
    "-mmin",
    "-maxdepth",
    "-mindepth",
    "-regex",
];
/// `find` 表达式里不取值的谓词与运算符（纯测试，加 `-delete` 动作）。
pub const FIND_FLAG_PREDICATES: &[&str] = &[
    "-print", "-print0", "-prune", "-empty", "-not", "-o", "-a", "-and", "-or", "(", ")", "!",
    "-delete",
];
/// `find` 取路径值的谓词（`-newer FILE` 读它）。
pub const FIND_PATH_PREDICATES: &[&str] = &["-newer"];
/// `find` 的执行类谓词：到 `;` 或紧跟在 `{}` 后面的 `+` 为止是一段，段里的词属于被执行的命令。
pub const FIND_EXEC_PREDICATES: &[&str] = &["-exec", "-execdir", "-ok", "-okdir"];

/// `dd` 允许的键：`if=` 读、`of=` 写，其余取非路径值。
pub const DD_KEYS: &[(&str, Option<Role>)] = &[
    ("if", Some(R)),
    ("of", Some(W)),
    ("bs", None),
    ("ibs", None),
    ("obs", None),
    ("count", None),
    ("skip", None),
    ("seek", None),
    ("status", None),
];

/// 表外程序登记的路径选项（不进白名单，只告诉参数规则「这个短选项取值、值是什么角色」）。
/// 没登记的短选项带粘值（`-Xvalue`）一律整条解析不了（方案 3.4、第 4 轮 1/F-12）。
/// 一个程序登记的路径选项：(短选项字母, 值的角色)。
pub type PathOptions = &'static [(char, &'static [Role])];

pub const PATH_OPTIONS: &[(&str, PathOptions)] = &[
    ("ssh", &[('E', &[W]), ('F', &[R]), ('i', &[R])]),
    ("curl", &[('o', &[W]), ('K', &[R])]),
    ("wget", &[('O', &[W]), ('i', &[R])]),
];

pub fn path_options(program: &str) -> PathOptions {
    PATH_OPTIONS
        .iter()
        .find(|(p, _)| *p == program)
        .map_or(&[], |(_, o)| o)
}

/// 出现在任何子命令里就让整条解析不了：以别的身份或别的方式执行、改变工作目录、程序文本来自参数。
pub const UNPARSABLE_PROGRAMS: &[&str] = &[
    "cd",
    "pushd",
    "popd",
    "command",
    "exec",
    "nohup",
    "time",
    "nice",
    "env",
    "xargs",
    "sudo",
    "doas",
    "su",
    "eval",
    "source",
    ".",
    "builtin",
    "trap",
    "alias",
    "export",
    "unset",
    "set",
    "declare",
    "typeset",
    "local",
    "readonly",
    // 包装词：后面的词是另一条命令（`timeout 5 rm -rf src` 的删除被藏在参数里）。方案 3.4 的清单漏了这些，
    // 同类补登记（T07 收口实施时发现）
    "timeout",
    "gtimeout",
    "stdbuf",
    "ionice",
    "setsid",
    "caffeinate",
    "flock",
    "chrt",
    "taskset",
    "unbuffer",
    "watch",
    "chroot",
    "nsenter",
    "arch",
];

/// shell：带 `-c` 一类选项（程序文本来自参数）时整条解析不了；不带时是读标准输入或跑脚本（执行脚本）。
pub const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh", "fish", "csh", "tcsh"];

/// 解释器：这些选项让程序文本来自参数，整条解析不了。
pub const INLINE_CODE_OPTIONS: &[(&str, &[&str])] = &[
    ("python", &["-c"]),
    ("python2", &["-c"]),
    ("python3", &["-c"]),
    ("node", &["-e", "-p", "--eval", "--print"]),
    ("deno", &["eval"]),
    ("bun", &["-e", "--eval"]),
    ("perl", &["-e", "-E"]),
    ("ruby", &["-e"]),
    ("php", &["-r"]),
    ("lua", &["-e"]),
    ("osascript", &["-e"]),
];

/// 执行脚本类：审批看不到脚本内部（卡片标注，方案 3.4、7.3 已知缺口）。
pub const OPAQUE_PROGRAMS: &[&str] = &[
    "bash",
    "sh",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "python",
    "python2",
    "python3",
    "node",
    "deno",
    "bun",
    "ruby",
    "perl",
    "php",
    "lua",
    "make",
    "cmake",
    "ninja",
    "just",
    "npx",
    "osascript",
];

/// 执行脚本类的子命令（`npm run x`、`cargo run`）。
pub const OPAQUE_SUBCOMMANDS: &[(&str, &str)] = &[
    ("npm", "run"),
    ("npm", "exec"),
    ("pnpm", "run"),
    ("pnpm", "exec"),
    ("pnpm", "dlx"),
    ("yarn", "run"),
    ("yarn", "dlx"),
    ("bun", "run"),
    ("bun", "x"),
    ("cargo", "run"),
    ("go", "run"),
    ("uv", "run"),
    ("poetry", "run"),
];

/// 发布类（方案 3.4，第 2 轮复审 2/F-1）：程序 + 子命令序列。在执行兜底之前产生 `Publish`。
pub const PUBLISH_COMMANDS: &[(&str, &[&str])] = &[
    ("npm", &["publish"]),
    ("pnpm", &["publish"]),
    ("yarn", &["publish"]),
    ("yarn", &["npm", "publish"]),
    ("cargo", &["publish"]),
    ("twine", &["upload"]),
    ("gem", &["push"]),
    ("docker", &["push"]),
    ("gh", &["release", "create"]),
    ("gh", &["release", "upload"]),
    ("flutter", &["pub", "publish"]),
    ("dart", &["pub", "publish"]),
    ("mvn", &["deploy"]),
    ("gradle", &["publish"]),
    ("nuget", &["push"]),
    ("dotnet", &["nuget", "push"]),
    ("helm", &["push"]),
];

/// `git` 子命令前允许的全局选项（不取值）。
pub const GIT_GLOBAL_FLAGS: &[&str] = &[
    "--no-pager",
    "-p",
    "--paginate",
    "-P",
    "--bare",
    "--no-replace-objects",
    "--literal-pathspecs",
    "--glob-pathspecs",
    "--noglob-pathspecs",
    "--icase-pathspecs",
    "--no-optional-locks",
];

/// `git` 改变工作目录或仓库位置的全局选项：之后的相对路径无从判断，整条解析不了。
pub const GIT_RELOCATING: &[&str] = &["-C", "--git-dir", "--work-tree", "--namespace"];
