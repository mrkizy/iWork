/* iWork T04 N5 的 macOS 映像探针（方案 8 N5 的 B3、B4、D 组）。由 iwork-t04-bundle-probe.sh 编译。
 *
 * 取法与 crates/iwork-ipc/src/peer/macos.rs 的 own_image 相同：SecCodeCopySelf → 读签名信息里的 cdhash
 * （kSecCodeInfoUnique）→ 钉成 `cdhash H"…"` 要求 → SecCodeCheckValidity。证书签名的进程照样取 cdhash。
 *
 *   imgprobe self [N]      打印运行映像的 cdhash（running=…）与按自己路径取的静态 cdhash（static=…），
 *                          之后每秒重取一次，共 N 次（默认 0）：看运行中换掉路径上的文件后两者各自怎么变
 *   imgprobe static PATH   只读签名里的静态 cdhash（不做校验），打印耗时
 *   imgprobe verify APP    整包静态验签：严格 + 嵌套代码 + 全部架构，打印结果与耗时
 *   imgprobe selfcheck     对自己的运行映像做 macos.rs check_image_file 那一步（SecCodeCopyStaticCode →
 *                          SecStaticCodeCheckValidity），分别用默认参数与 kSecCSDoNotValidateResources
 *
 * MARK 宏只用来造「同一份源码、不同构建标记」的两个二进制（cdhash 不同）。
 */
#include <CoreFoundation/CoreFoundation.h>
#include <Security/Security.h>
#include <mach/mach_time.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#ifndef MARK
#define MARK "0"
#endif
const char *volatile build_mark = "iwork-t04-imgprobe-mark-" MARK;

static double now_ms(void) {
  static mach_timebase_info_data_t tb;
  if (!tb.denom) mach_timebase_info(&tb);
  return (double)mach_absolute_time() * tb.numer / tb.denom / 1e6;
}

static void hex(CFDataRef d, char *out) {
  const UInt8 *p = CFDataGetBytePtr(d);
  for (CFIndex i = 0; i < CFDataGetLength(d); i++) sprintf(out + 2 * i, "%02x", p[i]);
}

/* 签名信息里的 cdhash；path_out 非空时顺带给出主可执行文件路径。 */
static OSStatus cdhash_of(SecStaticCodeRef code, char *out, char *path_out, size_t path_len) {
  CFDictionaryRef info = NULL;
  OSStatus st = SecCodeCopySigningInformation(code, kSecCSSigningInformation, &info);
  if (st) return st;
  CFDataRef uniq = CFDictionaryGetValue(info, kSecCodeInfoUnique);
  if (!uniq) { CFRelease(info); return -1; }
  hex(uniq, out);
  if (path_out) {
    CFURLRef url = CFDictionaryGetValue(info, kSecCodeInfoMainExecutable);
    if (!url || !CFURLGetFileSystemRepresentation(url, true, (UInt8 *)path_out, (CFIndex)path_len))
      path_out[0] = 0;
  }
  CFRelease(info);
  return 0;
}

/* 运行映像的 cdhash：与 macos.rs 同一串调用。 */
static OSStatus running_id(char *out, char *path, size_t path_len) {
  SecCodeRef self = NULL;
  OSStatus st = SecCodeCopySelf(kSecCSDefaultFlags, &self);
  if (st) return st;
  st = cdhash_of((SecStaticCodeRef)self, out, path, path_len);
  if (!st) {
    char req[128];
    snprintf(req, sizeof req, "cdhash H\"%s\"", out);
    CFStringRef rs = CFStringCreateWithCString(NULL, req, kCFStringEncodingUTF8);
    SecRequirementRef r = NULL;
    st = SecRequirementCreateWithString(rs, kSecCSDefaultFlags, &r);
    if (!st) st = SecCodeCheckValidity(self, kSecCSDefaultFlags, r);
    if (r) CFRelease(r);
    CFRelease(rs);
  }
  CFRelease(self);
  return st;
}

static OSStatus static_id(const char *path, char *out) {
  CFURLRef url = CFURLCreateFromFileSystemRepresentation(NULL, (const UInt8 *)path, (CFIndex)strlen(path), false);
  SecStaticCodeRef code = NULL;
  OSStatus st = SecStaticCodeCreateWithPath(url, kSecCSDefaultFlags, &code);
  CFRelease(url);
  if (st) return st;
  st = cdhash_of(code, out, NULL, 0);
  CFRelease(code);
  return st;
}

int main(int argc, char **argv) {
  char id[128] = {0}, sid[128] = {0}, path[2048] = {0};
  if (argc >= 2 && !strcmp(argv[1], "self")) {
    int n = argc >= 3 ? atoi(argv[2]) : 0;
    OSStatus st = running_id(id, path, sizeof path);
    printf("pid=%d mark=%s\n", getpid(), build_mark);
    printf("running=%s status=%d path=%s\n", st ? "-" : id, (int)st, path);
    OSStatus ss = static_id(path, sid);
    printf("static=%s status=%d equal=%s\n", ss ? "-" : sid, (int)ss, (!st && !ss && !strcmp(id, sid)) ? "yes" : "no");
    fflush(stdout);
    for (int i = 0; i < n; i++) {
      sleep(1);
      char r2[128] = {0}, s2[128] = {0};
      OSStatus a = running_id(r2, NULL, 0), b = static_id(path, s2);
      printf("tick=%d running=%s(%d) static=%s(%d)\n", i, a ? "-" : r2, (int)a, b ? "-" : s2, (int)b);
      fflush(stdout);
    }
    return st ? 1 : 0;
  }
  if (argc == 3 && !strcmp(argv[1], "static")) {
    double t = now_ms();
    OSStatus st = static_id(argv[2], sid);
    printf("static=%s status=%d ms=%.3f\n", st ? "-" : sid, (int)st, now_ms() - t);
    return st ? 1 : 0;
  }
  if (argc == 3 && !strcmp(argv[1], "verify")) {
    CFURLRef url = CFURLCreateFromFileSystemRepresentation(NULL, (const UInt8 *)argv[2], (CFIndex)strlen(argv[2]), true);
    SecStaticCodeRef code = NULL;
    double t = now_ms();
    OSStatus st = SecStaticCodeCreateWithPath(url, kSecCSDefaultFlags, &code);
    CFErrorRef err = NULL;
    if (!st)
      st = SecStaticCodeCheckValidityWithErrors(
          code, kSecCSStrictValidate | kSecCSCheckNestedCode | kSecCSCheckAllArchitectures, NULL, &err);
    double ms = now_ms() - t;
    char msg[1024] = "";
    if (err) {
      CFStringRef d = CFErrorCopyDescription(err);
      CFStringGetCString(d, msg, sizeof msg, kCFStringEncodingUTF8);
      CFRelease(d);
      CFRelease(err);
    }
    printf("verify=%d ms=%.1f %s\n", (int)st, ms, msg);
    if (code) CFRelease(code);
    CFRelease(url);
    return st ? 1 : 0;
  }
  if (argc == 2 && !strcmp(argv[1], "selfcheck")) {
    SecCodeRef self = NULL;
    SecStaticCodeRef file = NULL;
    OSStatus st = SecCodeCopySelf(kSecCSDefaultFlags, &self);
    if (!st) st = SecCodeCopyStaticCode(self, kSecCSDefaultFlags, &file);
    if (st) { printf("copy=%d\n", (int)st); return 1; }
    printf("static_default=%d static_no_resources=%d dynamic=%d\n",
           (int)SecStaticCodeCheckValidity(file, kSecCSDefaultFlags, NULL),
           (int)SecStaticCodeCheckValidity(file, kSecCSDoNotValidateResources, NULL),
           (int)SecCodeCheckValidity(self, kSecCSDefaultFlags, NULL));
    return 0;
  }
  fprintf(stderr, "用法：imgprobe self [N] | static PATH | verify APP | selfcheck\n");
  return 2;
}
