# ADR-0010 项目身份规则放 core，项目标记与新建目录各走一个新端口

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
    进程把某一级换成链接，仍可能读穿。静态的恶意仓库内容挡得住，专门抢这个时间窗的并发进程挡不住。产品负责人已确认登记为
    已知限制（需求 v1.70，第 9 章）。
- **后台读操作不运行任何外部程序**（T08 审查第 2 轮 2/F-1，取代第 1 轮「diff 运行过滤器」的决定）：仓库配置（含 `.git/config`，
  Agent 的脚本可能写过它）里的 `filter.<name>.clean` / `smudge` / `process` 会让 gix 在比较内容时以后台服务的权限、在沙箱外
  运行任意程序，等于绕过审批与沙箱。所以 `iwork-vcs-git` 的读操作（`baseline`、`status`、`diff` 的全部阶段，包括候选扫描与改名
  比较）只经一个入口 `workspace::open` 打开仓库：打开后从**内存里的**配置删掉全部 `filter.*` 节（磁盘上的配置不动），再确认过滤
  管线里没有外部驱动，否则报错。进程内的换行、编码、ident 转换照常，两侧都换成 git 内部表示再比较。
  - **需要外部过滤器的已跟踪文件**（第 3 轮 3/F-1，需求 v1.71 第 9 章）：原始字节与存储内容相同也不能说明没改动（存储的是过滤后的
    内容），所以不靠内容判断。在候选筛选之前按 `filter` 属性识别这类路径，只看暂存区记的 stat（时间戳、大小等）：变了就列为
    「可能有改动」，差异里只写「内容比较不支持」并在已存内容是短文本时（例如 LFS 指针）列出它，增删行数为 0；stat 没变就算没改动。
    `status` 与 `diff` 共用同一个判定（`Worktree::changes_gix_cannot_judge`）。未跟踪的这类文件照常列为新文件，差异同样只标不支持。
  - **子模块**（第 3 轮 2/F-1，需求 v1.71 第 9 章）：gix 的子模块扫描会用子模块自己未清理的配置跑 status，所以关掉
    （`index_worktree_submodules(None)`），不显示子模块内部未提交的改动，只比较检出的提交与暂存区记录的提交（相当于
    `--ignore-submodules=dirty`）；读子模块 HEAD 也经 `workspace::open`，子模块的配置同样先去掉外部过滤器。
  - 提交之间的比较两侧都是已存的内容（LFS 是指针），照常比较，能看到 oid、size 的变化。
  - `init`、`clone` 是用户主动请求的写操作，按技术设计沿用用户的 git 配置，不在此列。

## 备选

- 项目标记读写与目录准备塞进 `iwork-vcs-git`（不加端口）：改动少，但项目身份绑在 git 上，违反「版本控制差异收在 VcsProvider」。已否决。
- 给 `Project` 加 `shared_dir` 字段，去重直接比投影：每次添加少几次 `detect`，但要改 T01 的实体、事件与投影；仓库被移动后存下的值也会过时。暂不采用，项目数上来再议。

- `iwork-rootfs` 用 `openat` 系列（经 libc 或 cap-std）彻底消除 TOCTOU：要引入外部依赖且三平台各写一套，首版不做；
  产品负责人不接受上面的已知限制时再议。
- 读操作照常运行配置的过滤器（与 `git diff`、`git status` 一致）：Agent 能写 `.git/config` 时，后台服务会替它在沙箱外运行任意程序。已否决（第 1 轮曾采用，第 2 轮推翻）。

## 影响

- T10 的添加项目、重新定位、启动时路径检查直接调 `project_identity` 的函数，把结果翻成事件。
- 换一种版本控制工具只需新的 `VcsProvider` 实现，项目身份规则与标记文件不变。
