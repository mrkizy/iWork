# ADR-0006 项目身份规则放 core，项目标记与新建目录各走一个新端口

- 状态：已采纳（2026-11-09，T08；supervisor 裁定方案 A）
- 依据：技术设计 8.2、9.3；需求 FR19；`AGENTS.md`「核心不写 git 特例」「依赖只向下」；T01 方案 5.2「端口由第一个需要它的卡加」

## 背景

T08 要实现项目身份：首次添加写 `.iwork/project.toml`，按共享目录去重，按项目 ID 重新定位，检查路径是否还在；新建项目时还要准备目录。这些都要做 I/O，但：

- `iwork-domain`、`iwork-core` 不许做 I/O；
- 项目标记与新建目录跟版本控制无关，放进 `iwork-vcs-git` 会把项目身份绑到 git 上，换一种版本控制工具时要重写；
- `iwork-app` 只能依赖 core、ports、proto、domain，调不到基础设施 crate，所以光放在 `iwork-host` 里不加端口，T10 的命令受理用不上。

## 决定

- **规则**放 `iwork-core::project_identity`（`resolve_add`、`assign_id`、`check_path`、`relocate`），只经端口做 I/O：
  - 仓库归属与共享目录问 `VcsProvider::detect`；路径不在了即 `VcsError::PathMissing`，不另加端口。
  - 去重时对已登记项目的根重新 `detect` 取共享目录，不给 `Project` 加字段（不改 T01 的定义）。个人工具的项目数是几十个，代价可忽略。
  - 共享目录的「真实路径」由 VCS 实现用 `canonicalize` 求：解析符号链接，并在大小写不敏感的盘上换成磁盘上的写法（macOS 的 realpath、Windows 的 `GetFinalPathNameByHandle` 都这样做）。不做小写化：在大小写敏感的盘上那会把两个目录误合并。
- **新端口**（`iwork-ports::project_files`）：
  - `ProjectMarkerStore`：读 / 新建项目标记；已存在时绝不覆盖。项目 ID 只属于本机这份工作副本、不进版本控制（产品负责人 2026-11-09 决定）：host 实现写标记前先在 `.iwork/.gitignore` 里加 `/project.toml`（文件不存在就建，已有就保留原内容追加一行，已有这一行就不动）。忽略文件写在 host 而不在 `iwork-vcs-git`：它跟项目标记是一件事，而且只是写一个文件，不调用 git。
  - `ProjectDirectories`：新建项目时在父目录下准备空目录（父目录不存在则建，目标非空拒绝，名字必须是单独一级目录名）。
  - 实现都在 `iwork-host::project_files`（解析 TOML 用 `toml` crate）。默认父目录 `~/iWork/projects` 由 `iwork_host::project_files::default_projects_root()` 给出，作为全局设置 `projects_root` 的初始值，daemon 组合时读取。
- **领域方法**：`Project::touch`、`Project::set_pinned`、`project::sidebar_order`，只加方法，不改已有定义。
- **ID 冲突兜底**：标记默认被忽略；万一用户强制提交了它、仓库又被另克隆一份，标记里的 ID 已登记给另一个位置的项目，`resolve_add` 返回 `IdConflict`（对外 `PROJECT_ID_CONFLICT`，带冲突的 ID 与已登记项目的路径），不覆盖、不换 ID、也不当成重新定位。

- **项目根内的文件读写**（T08 审查第 1 轮 F-1、F-2，supervisor 同意）：新建只用 std、零外部依赖的工作区 crate `iwork-rootfs`，
  `iwork-vcs-git`（读工作区文件出差异）与 `iwork-host`（读写项目标记与忽略文件）共用。理由：两者按依赖方向互不依赖，
  `ports`、`domain` 不放实现，复制两份又违反一处真值。规则：相对路径不许带 `..` 或绝对路径；路径上任何一级都不跟随
  符号链接（中间一级是链接直接拒绝，最后一级是链接时只能取链接目标文本）；新建用独占创建，替换用「临时文件 + 改名」。
  - **已知限制（TOCTOU）**：只用 std 做不到 `openat` / `O_NOFOLLOW`，检查与打开是两步。检查之后、打开之前若有并发运行的
    进程把某一级换成链接，仍可能读穿。静态的恶意仓库内容挡得住，专门抢这个时间窗的并发进程挡不住。是否接受由产品负责人确认。
- **差异的内容表示**（F-7）：两侧都换成 git 内部表示再比较，工作区文件过 gix 的内容过滤管线（换行转换、编码、用户配置的
  clean 过滤器），与 `git diff` 一致；过滤器失败（例如必需的 LFS 程序不存在）时整个 `diff` 返回错误，不生成错误的差异。
  gix 的 `status` 本来就按同一套配置运行 clean 过滤器（与 `git status` 一样），`diff` 与它保持一致。

## 备选

- 项目标记读写与目录准备塞进 `iwork-vcs-git`（不加端口）：改动少，但项目身份绑在 git 上，违反「版本控制差异收在 VcsProvider」。已否决。
- 给 `Project` 加 `shared_dir` 字段，去重直接比投影：每次添加少几次 `detect`，但要改 T01 的实体、事件与投影；仓库被移动后存下的值也会过时。暂不采用，项目数上来再议。

- `iwork-rootfs` 用 `openat` 系列（经 libc 或 cap-std）彻底消除 TOCTOU：要引入外部依赖且三平台各写一套，首版不做；
  产品负责人不接受上面的已知限制时再议。
- 差异里不运行内容过滤器、遇到就标注「不支持」：LFS 文件的每次比较都会显示为整文件改动（指针对内容），而且与 `status`
  的判断不一致。已否决。

## 影响

- T10 的添加项目、重新定位、启动时路径检查直接调 `project_identity` 的函数，把结果翻成事件。
- 换一种版本控制工具只需新的 `VcsProvider` 实现，项目身份规则与标记文件不变。
