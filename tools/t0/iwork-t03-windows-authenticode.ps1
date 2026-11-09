# N2（docs/plans/T03.md 8.2）：Windows 上的自签 Authenticode、WinVerifyTrust 返回码、
# 改名后的进程映像路径、命名管道两端 pid。结论写进 docs/design/t0-findings.md「T03 本机通信实测 · Windows」。
#
# 用法（仓库根目录，普通用户的 PowerShell 5.1 或 7）：
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools/t0/iwork-t03-windows-authenticode.ps1
# 输出：tools/t0/out/t03-windows/（transcript.txt 是完整记录）。
#
# 决定性的一项是 ⑥(b)：改名运行中进程的 exe 后，QueryFullProcessImageNameW(PROCESS_NAME_NATIVE) 返回新名
# （live.old.exe）→ 绑定成立，可以把 crates/iwork-ipc/src/peer/windows_image.rs 的 IMAGE_BINDING_VERIFIED 改成 true；
# 返回旧名（live.exe）→ 绑定不成立，Windows 维持「M1 暂不支持」。

$ErrorActionPreference = 'Stop'
$out = Join-Path $PSScriptRoot 'out\t03-windows'
New-Item -ItemType Directory -Force $out | Out-Null
Start-Transcript -Path (Join-Path $out 'transcript.txt') -Force | Out-Null

Add-Type -TypeDefinition @'
using System;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using System.Text;

public static class N2 {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct WINTRUST_FILE_INFO {
        public uint cbStruct; public string pcwszFilePath; public IntPtr hFile; public IntPtr pgKnownSubject;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct WINTRUST_DATA {
        public uint cbStruct; public IntPtr pPolicyCallbackData; public IntPtr pSIPClientData;
        public uint dwUIChoice; public uint fdwRevocationChecks; public uint dwUnionChoice; public IntPtr pFile;
        public uint dwStateAction; public IntPtr hWVTStateData; public IntPtr pwszURLReference;
        public uint dwProvFlags; public uint dwUIContext; public IntPtr pSignatureSettings;
    }
    [DllImport("wintrust.dll")]
    static extern int WinVerifyTrust(IntPtr hwnd, ref Guid action, ref WINTRUST_DATA data);

    // 与 crates/iwork-ipc/src/peer/windows.rs 的参数相同：WTD_UI_NONE、WTD_REVOKE_NONE、
    // WTD_STATEACTION_VERIFY、WTD_CACHE_ONLY_URL_RETRIEVAL。
    public static string Verify(string path) {
        var fi = new WINTRUST_FILE_INFO();
        fi.cbStruct = (uint)Marshal.SizeOf(typeof(WINTRUST_FILE_INFO));
        fi.pcwszFilePath = path;
        IntPtr pfi = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(WINTRUST_FILE_INFO)));
        Marshal.StructureToPtr(fi, pfi, false);
        var d = new WINTRUST_DATA();
        d.cbStruct = (uint)Marshal.SizeOf(typeof(WINTRUST_DATA));
        d.dwUIChoice = 2; d.fdwRevocationChecks = 0; d.dwUnionChoice = 1; d.pFile = pfi;
        d.dwStateAction = 1; d.dwProvFlags = 0x1000;
        var action = new Guid("00AAC56B-CD44-11d0-8CC2-00C04FC295EE");
        int rc = WinVerifyTrust(IntPtr.Zero, ref action, ref d);
        d.dwStateAction = 2;
        WinVerifyTrust(IntPtr.Zero, ref action, ref d);
        Marshal.DestroyStructure(pfi, typeof(WINTRUST_FILE_INFO));
        Marshal.FreeHGlobal(pfi);
        return string.Format("0x{0:X8}", rc);
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool QueryFullProcessImageNameW(IntPtr h, uint flags, StringBuilder name, ref uint size);
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr h);

    // PROCESS_QUERY_LIMITED_INFORMATION + PROCESS_NAME_NATIVE，与 Rust 实现相同。
    public static string NativeImageName(int pid) {
        IntPtr h = OpenProcess(0x1000, false, pid);
        if (h == IntPtr.Zero) return "OpenProcess 失败：" + Marshal.GetLastWin32Error();
        try {
            var sb = new StringBuilder(32768);
            uint size = (uint)sb.Capacity;
            if (!QueryFullProcessImageNameW(h, 1, sb, ref size)) return "查名失败：" + Marshal.GetLastWin32Error();
            return sb.ToString(0, (int)size);
        } finally { CloseHandle(h); }
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetNamedPipeClientProcessId(IntPtr pipe, out uint pid);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetNamedPipeServerProcessId(IntPtr pipe, out uint pid);

    // ⑤：两端句柄各问一次对端 pid。同一进程里两端，期望都等于本进程 pid。
    public static string PipePids() {
        string name = "iwork-n2-" + Guid.NewGuid().ToString("N");
        using (var server = new NamedPipeServerStream(name, PipeDirection.InOut, 1))
        using (var client = new NamedPipeClientStream(".", name, PipeDirection.InOut)) {
            client.Connect(2000);
            server.WaitForConnection();
            uint seenByServer, seenByClient;
            bool a = GetNamedPipeClientProcessId(server.SafePipeHandle.DangerousGetHandle(), out seenByServer);
            bool b = GetNamedPipeServerProcessId(client.SafePipeHandle.DangerousGetHandle(), out seenByClient);
            return string.Format("server 看到客户端 pid={0}（ok={1}），client 看到服务端 pid={2}（ok={3}），本进程 pid={4}",
                seenByServer, a, seenByClient, b, System.Diagnostics.Process.GetCurrentProcess().Id);
        }
    }
}
'@

function Sha256Hex([byte[]] $bytes) {
    $sha = [Security.Cryptography.SHA256]::Create()
    try { ([BitConverter]::ToString($sha.ComputeHash($bytes)) -replace '-', '').ToLowerInvariant() } finally { $sha.Dispose() }
}

"== 环境"
"OS: $([Environment]::OSVersion.VersionString)  PowerShell: $($PSVersionTable.PSVersion)"

$cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=iWork Test Signer' -CertStoreLocation Cert:\CurrentUser\My
$live = $null
try {
    # 任何小 exe 都行；cmd.exe 本身是目录签名（不内嵌签名），拷出来就是「未签名」。
    Copy-Item (Get-Command cmd.exe).Source "$out\unsigned.exe" -Force
    Copy-Item "$out\unsigned.exe" "$out\signed.exe" -Force
    Set-AuthenticodeSignature -FilePath "$out\signed.exe" -Certificate $cert | Format-List Status, StatusMessage
    Copy-Item "$out\signed.exe" "$out\tampered.exe" -Force
    $bytes = [IO.File]::ReadAllBytes("$out\tampered.exe")
    $bytes[0x200] = $bytes[0x200] -bxor 1
    [IO.File]::WriteAllBytes("$out\tampered.exe", $bytes)

    "== ①②③ WinVerifyTrust 返回码（期望：signed=0x800B0109 CERT_E_UNTRUSTEDROOT，tampered=0x80096010 TRUST_E_BAD_DIGEST，unsigned=0x800B0100 TRUST_E_NOSIGNATURE）"
    foreach ($f in 'signed', 'tampered', 'unsigned') {
        "{0}: {1}" -f $f, [N2]::Verify((Resolve-Path "$out\$f.exe").Path)
    }

    "== ④ 叶证书 SHA-256（期望两行相同；Rust 侧算的是同一个值：叶证书 DER 的 SHA-256）"
    "证书本身：      $(Sha256Hex $cert.RawData)"
    "从签名里取回：  $(Sha256Hex (Get-AuthenticodeSignature "$out\signed.exe").SignerCertificate.RawData)"

    "== ⑤ 命名管道两端 pid"
    [N2]::PipePids()

    "== ⑥ 反例：对运行中的进程改它的 exe"
    Copy-Item "$out\signed.exe" "$out\live.exe" -Force
    $live = Start-Process -FilePath "$out\live.exe" -ArgumentList '/k' -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 1
    "启动后进程映像（NATIVE）：$([N2]::NativeImageName($live.Id))"
    try {
        [IO.File]::OpenWrite("$out\live.exe").Close()
        "(a) 原地写入成功：系统没有锁住运行中的映像，绑定不成立"
    } catch {
        "(a) 原地写入被拒（期望 ERROR_SHARING_VIOLATION）：$($_.Exception.Message)"
    }
    Rename-Item "$out\live.exe" 'live.old.exe'
    Copy-Item "$out\unsigned.exe" "$out\live.exe" -Force
    $after = [N2]::NativeImageName($live.Id)
    "(b) 改名并在原路径放入别的文件后，进程映像（NATIVE）：$after"
    if ($after -like '*\live.old.exe') {
        "(b) 结论：返回新名 → 绑定成立，可以把 IMAGE_BINDING_VERIFIED 改为 true（先写 t0-findings）"
    } else {
        "(b) 结论：没有返回新名 → 绑定不成立，Windows 维持 M1 暂不支持"
    }
    "(c) 两次查名之间换文件：由 Rust 的 2.4b 第③步拦（peer::windows_image::image_paths_agree 的单测覆盖判定；"
    "    真实竞争窗口在 Windows 上跑：cargo test -p iwork-ipc --target x86_64-pc-windows-msvc，并用 dev example peer-probe 对 live.old.exe 进程探）"
} finally {
    if ($live) { Stop-Process -Id $live.Id -Force -ErrorAction SilentlyContinue }
    Remove-Item "Cert:\CurrentUser\My\$($cert.Thumbprint)" -ErrorAction SilentlyContinue
    Stop-Transcript | Out-Null
}
