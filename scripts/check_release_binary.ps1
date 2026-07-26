<#
  Fail-closed validation for the exact QuickDictate executable published to GitHub.
  It verifies PE structure, GUI subsystem, version metadata, embedded icon resources,
  and the side-effect-free --version canary used by the self-updater.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$ExePath,

    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$')]
    [string]$ExpectedVersion
)

$ErrorActionPreference = 'Stop'
$exe = Get-Item -LiteralPath $ExePath -ErrorAction Stop
if ($exe.PSIsContainer) { throw "release executable is a directory: $ExePath" }

$bytes = [System.IO.File]::ReadAllBytes($exe.FullName)
if ($bytes.Length -lt 256 -or $bytes[0] -ne 0x4d -or $bytes[1] -ne 0x5a) {
    throw 'release executable has no valid MZ header'
}
$peOffset = [BitConverter]::ToInt32($bytes, 0x3c)
if ($peOffset -lt 0 -or $peOffset + 94 -ge $bytes.Length -or
    [BitConverter]::ToUInt32($bytes, $peOffset) -ne 0x00004550) {
    throw 'release executable has no valid PE header'
}
$subsystem = [BitConverter]::ToUInt16($bytes, $peOffset + 92)
if ($subsystem -ne 2) {
    throw "release executable uses PE subsystem $subsystem, expected Windows GUI (2)"
}

$version = $exe.VersionInfo
if (([string]$version.ProductName).Trim() -cne 'QuickDictate') {
    throw "ProductName is '$($version.ProductName)', expected QuickDictate"
}
if (([string]$version.ProductVersion).Trim() -cne $ExpectedVersion) {
    throw "ProductVersion is '$($version.ProductVersion)', expected $ExpectedVersion"
}
if (([string]$version.FileDescription).Trim().Length -eq 0) {
    throw 'FileDescription is missing'
}

if (-not ('QuickDictateReleaseResourceAudit' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class QuickDictateReleaseResourceAudit {
    private delegate bool EnumResourceName(IntPtr module, IntPtr type, IntPtr name, IntPtr state);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr LoadLibraryEx(string path, IntPtr file, uint flags);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool FreeLibrary(IntPtr module);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool EnumResourceNames(
        IntPtr module, IntPtr type, EnumResourceName callback, IntPtr state);

    public static int GroupIconCount(string path) {
        const uint LOAD_LIBRARY_AS_DATAFILE = 0x00000002;
        const uint LOAD_LIBRARY_AS_IMAGE_RESOURCE = 0x00000020;
        IntPtr module = LoadLibraryEx(
            path, IntPtr.Zero, LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE);
        if (module == IntPtr.Zero) return -1;
        int count = 0;
        EnumResourceName callback = delegate { count++; return true; };
        EnumResourceNames(module, new IntPtr(14), callback, IntPtr.Zero); // RT_GROUP_ICON
        FreeLibrary(module);
        GC.KeepAlive(callback);
        return count;
    }
}
'@
}
$iconGroups = [QuickDictateReleaseResourceAudit]::GroupIconCount($exe.FullName)
if ($iconGroups -lt 1) {
    throw 'release executable has no embedded RT_GROUP_ICON resource'
}

$reportedVersion = (& $exe.FullName --version | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $reportedVersion -cne $ExpectedVersion) {
    throw "--version reported '$reportedVersion' (exit $LASTEXITCODE), expected $ExpectedVersion"
}

$sha256 = (Get-FileHash -LiteralPath $exe.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "[release-binary] PASS - QuickDictate $ExpectedVersion, GUI subsystem, $iconGroups icon group(s), sha256:$sha256"
