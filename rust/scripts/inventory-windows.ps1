$ErrorActionPreference = 'Stop'
# Read-only inventory: no installers, caches, builds, secrets or system changes.
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
$installations = & $vswhere -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -format json | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'vswhere failed' }
$records = @()
foreach ($installation in $installations) {
    $tools = @(Get-ChildItem -Directory (Join-Path $installation.installationPath 'VC/Tools/MSVC') | Select-Object -ExpandProperty Name)
    $sdks = @(Get-ChildItem -Directory (Join-Path ${env:ProgramFiles(x86)} 'Windows Kits/10/Lib') | Select-Object -ExpandProperty Name)
    $requestedTools = '14.44.35207'
    $requestedSdk = '10.0.26100.0'
    $selected = $null
    if ($tools -contains $requestedTools -and $sdks -contains $requestedSdk) {
        $script = Join-Path $installation.installationPath 'VC/Auxiliary/Build/vcvarsall.bat'
        $command = 'call "' + $script + '" amd64 ' + $requestedSdk + ' -vcvars_ver=' + $requestedTools + ' >nul && set'
        $lines = & $env:ComSpec /d /s /c $command
        if ($LASTEXITCODE -ne 0) { throw 'Exact vcvarsall selection failed' }
        $selected = @{}
        foreach ($line in $lines) {
            if ($line -match '^(VCToolsVersion|WindowsSDKVersion|VCToolsInstallDir|WindowsSdkDir|VisualStudioVersion)=(.*)$') {
                $selected[$Matches[1]] = $Matches[2]
            }
        }
        if ($selected['VCToolsVersion'].TrimEnd('\', '/') -ne $requestedTools) { throw 'Selected MSVC mismatch' }
        if ($selected['WindowsSDKVersion'].TrimEnd('\', '/') -ne $requestedSdk) { throw 'Selected SDK mismatch' }
        $compiler = Join-Path $installation.installationPath "VC/Tools/MSVC/$requestedTools/bin/Hostx64/x64/cl.exe"
        $linker = Join-Path $installation.installationPath "VC/Tools/MSVC/$requestedTools/bin/Hostx64/x64/link.exe"
        $selected['clSha256'] = (Get-FileHash -Algorithm SHA256 $compiler).Hash.ToLowerInvariant()
        $selected['linkSha256'] = (Get-FileHash -Algorithm SHA256 $linker).Hash.ToLowerInvariant()
        $selected['clFileVersion'] = (Get-Item $compiler).VersionInfo.FileVersion
    }
    $records += [ordered]@{
        visual_studio_version = $installation.installationVersion
        msvc_tools_directories = $tools
        windows_sdk_directories = $sdks
        requested_msvc_tools_version = $requestedTools
        requested_windows_sdk_version = $requestedSdk
        selected = $selected
    }
}
$result = [ordered]@{
    image_version = $env:ImageVersion
    image_os = $env:ImageOS
    os_version = [System.Environment]::OSVersion.Version.ToString()
    runner_architecture = $env:RUNNER_ARCH
    installations = $records
}
$result | ConvertTo-Json -Depth 8
if ($records.Count -ne 1 -or $null -eq $records[0].selected) {
    throw 'No uniquely observed complete existing compiler/SDK tuple; do not add a profile'
}
