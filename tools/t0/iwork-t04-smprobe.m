// iWork T04 N1（方案第 8 节）的 SMAppService / launchd 探针。由 iwork-t04-macos-smappservice.sh 编译，
// 同一个二进制在包里放两份：host（调 register / unregister / status）与 agent（被 launchd 起的那个）。
//
//   smprobe host register|unregister|status <plistName>
//   smprobe agent --service          launchd 起的实例。每次启动往 $T04_LOG 追加一行「start pid 参数 时间」，
//                                    再按 $T04_MODE 文件第一行决定做什么；sleep 期间每 0.2 秒重读，变了就照新模式做：
//     sleep             一直睡；收到 SIGTERM 记一行再按默认动作结束
//     exit N            以 N 退出
//     signal N          对自己发信号 N（9 KILL、15 TERM、11 SEGV）
//     abort             abort()
//     lock PATH         对 PATH 阻塞 flock，拿到后记一行，接着轮询
//     unregister NAME   自己调 SMAppService unregister，记返回与之后每 0.2 秒的存活
//     cmd SHELL...      system(SHELL)，记返回与之后每 0.2 秒的存活（退路的 launchctl bootout 用）
//     exec              exec 自己一次（参数加 --execd），新映像照常读 mode（N3 第 3 项）
#import <Foundation/Foundation.h>
#import <ServiceManagement/ServiceManagement.h>
#include <fcntl.h>
#include <mach-o/dyld.h>
#include <signal.h>
#include <sys/file.h>
#include <sys/time.h>
#include <unistd.h>

static const char *log_path(void) {
  const char *p = getenv("T04_LOG");
  return p ? p : "/tmp/t04-smprobe.log";
}

static double now_s(void) {
  struct timeval tv;
  gettimeofday(&tv, NULL);
  return tv.tv_sec + tv.tv_usec / 1e6;
}

// 只用 async-signal-safe 的调用也能写：信号处理函数里调它。
static void logline(const char *msg) {
  int fd = open(log_path(), O_WRONLY | O_APPEND | O_CREAT, 0600);
  if (fd < 0) return;
  char buf[512];
  int n = snprintf(buf, sizeof buf, "%.3f pid=%d %s\n", now_s(), getpid(), msg);
  write(fd, buf, n);
  fsync(fd);
  close(fd);
}

static void on_term(int sig) {
  logline(sig == SIGTERM ? "got SIGTERM" : "got signal");
  signal(sig, SIG_DFL);
  raise(sig);
}

static const char *status_name(SMAppServiceStatus s) {
  switch (s) {
    case SMAppServiceStatusNotRegistered: return "NotRegistered";
    case SMAppServiceStatusEnabled: return "Enabled";
    case SMAppServiceStatusRequiresApproval: return "RequiresApproval";
    case SMAppServiceStatusNotFound: return "NotFound";
  }
  return "?";
}

static int host(const char *op, const char *plist) {
  SMAppService *svc = [SMAppService agentServiceWithPlistName:[NSString stringWithUTF8String:plist]];
  NSError *err = nil;
  BOOL ok = YES;
  if (!strcmp(op, "register")) ok = [svc registerAndReturnError:&err];
  else if (!strcmp(op, "unregister")) ok = [svc unregisterAndReturnError:&err];
  else if (strcmp(op, "status")) return 2;
  printf("op=%s ok=%d status=%s", op, ok, status_name(svc.status));
  if (err) printf(" error_domain=%s error_code=%ld error=%s", err.domain.UTF8String, (long)err.code, err.localizedDescription.UTF8String);
  printf("\n");
  return ok ? 0 : 1;
}

static void sleep_forever(void) {
  for (;;) pause();
}

static void read_mode(char *mode, size_t len) {
  snprintf(mode, len, "sleep");
  const char *mp = getenv("T04_MODE");
  FILE *f = mp ? fopen(mp, "r") : NULL;
  if (f) { if (fgets(mode, (int)len, f)) mode[strcspn(mode, "\n")] = 0; fclose(f); }
}

static int g_argc; static char **g_argv;

/* 按模式行动。只有 sleep 与 lock（拿到锁之后）会返回。 */
static void act(const char *mode) {
  char m[300]; snprintf(m, sizeof m, "mode %s", mode); logline(m);
  int n = 0; char arg[200] = "";
  if (!strcmp(mode, "sleep")) return;
  if (sscanf(mode, "exit %d", &n) == 1) { logline("exiting"); exit(n); }
  if (sscanf(mode, "signal %d", &n) == 1) { logline("raising"); signal(n, SIG_DFL); raise(n); pause(); }
  if (!strcmp(mode, "abort")) { logline("aborting"); abort(); }
  if (sscanf(mode, "lock %199s", arg) == 1) {
    int fd = open(arg, O_RDWR | O_CREAT, 0600);
    logline("waiting lock");
    flock(fd, LOCK_EX);
    logline("got lock");
    return; /* 拿到锁后回到轮询，模式再变时照常行动（锁一直握着，进程不退） */
  }
  if (sscanf(mode, "unregister %199s", arg) == 1) {
    logline("calling unregister");
    SMAppService *svc = [SMAppService agentServiceWithPlistName:[NSString stringWithUTF8String:arg]];
    NSError *err = nil;
    BOOL ok = [svc unregisterAndReturnError:&err];
    char r[400]; snprintf(r, sizeof r, "unregister returned ok=%d code=%ld", ok, (long)err.code); logline(r);
    for (int i = 0; i < 100; i++) { usleep(200000); char a[64]; snprintf(a, sizeof a, "alive %d", i); logline(a); }
    logline("exiting 0 after unregister");
    exit(0);
  }
  if (!strncmp(mode, "cmd ", 4)) {
    logline("calling cmd");
    int rc = system(mode + 4);
    char r[64]; snprintf(r, sizeof r, "cmd returned %d", rc); logline(r);
    for (int i = 0; i < 100; i++) { usleep(200000); char a[64]; snprintf(a, sizeof a, "alive %d", i); logline(a); }
    logline("exiting 0 after cmd");
    exit(0);
  }
  if (!strcmp(mode, "exec")) {
    for (int i = 0; i < g_argc; i++) if (!strcmp(g_argv[i], "--execd")) sleep_forever();
    logline("exec self");
    char *nargv[g_argc + 2];
    for (int i = 0; i < g_argc; i++) nargv[i] = g_argv[i];
    nargv[g_argc] = "--execd"; nargv[g_argc + 1] = NULL;
    char self[4096]; uint32_t sz = sizeof self;
    if (_NSGetExecutablePath(self, &sz)) { logline("no exe path"); exit(21); }
    execv(self, nargv);
    logline("exec failed");
    exit(21);
  }
  logline("unknown mode");
  exit(3);
}

static int agent(int argc, char **argv) {
  g_argc = argc; g_argv = argv;
  signal(SIGTERM, on_term);
  char start[256] = "start";
  for (int i = 0; i < argc && strlen(start) < 200; i++) { strcat(start, " "); strcat(start, argv[i]); }
  logline(start);
  char mode[256];
  read_mode(mode, sizeof mode);
  act(mode);
  /* 每 0.2 秒重读模式，和上次不同就由这个正在跑的实例自己行动（不用 kickstart -k 换实例） */
  for (;;) {
    usleep(200000);
    char now[256];
    read_mode(now, sizeof now);
    if (strcmp(now, mode)) {
      snprintf(mode, sizeof mode, "%s", now);
      act(mode);
    }
  }
}

int main(int argc, char **argv) {
  @autoreleasepool {
    if (argc >= 4 && !strcmp(argv[1], "host")) return host(argv[2], argv[3]);
    for (int i = 1; i < argc; i++) if (!strcmp(argv[i], "--service")) return agent(argc, argv);
    fprintf(stderr, "usage: smprobe host register|unregister|status <plist> | smprobe --service\n");
    return 2;
  }
}
