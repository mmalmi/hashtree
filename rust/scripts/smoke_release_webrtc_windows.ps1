[CmdletBinding(DefaultParameterSetName = "Binary")]
param(
    [Parameter(Mandatory, ParameterSetName = "Binary")]
    [string]$BinaryPath,
    [Parameter(Mandatory, ParameterSetName = "Archive")]
    [string]$ArchivePath
)

$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $false
$smoke = Join-Path ([System.IO.Path]::GetTempPath()) ("hashtree-webrtc-smoke-" + [guid]::NewGuid())
$savedEnvironment = @{}
foreach ($name in @("HTREE_CONFIG_DIR", "HTREE_DATA_DIR", "RUST_LOG")) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
}

try {
    $config = Join-Path $smoke "config"
    $data = Join-Path $smoke "data"
    New-Item -ItemType Directory -Force -Path $config, $data | Out-Null
    if ($PSCmdlet.ParameterSetName -eq "Archive") {
        Expand-Archive -Path $ArchivePath -DestinationPath $smoke
        $BinaryPath = Join-Path $smoke "hashtree/htree.exe"
    }
    $env:HTREE_CONFIG_DIR = $config
    $env:HTREE_DATA_DIR = $data
    $env:RUST_LOG = "fips_core::transport::webrtc=info,fips_core::node::lifecycle::runtime=info"
    $output = (& $BinaryPath --data-dir $data start --addr 127.0.0.1:0 --relays "" 2>&1 | Out-String)
    Write-Output $output
    if ($output -notmatch "WebRTC transport started") {
        throw "htree did not start its FIPS WebRTC transport"
    }
    if ($output -match "built without the webrtc feature") {
        throw "htree lacks the required FIPS WebRTC feature"
    }
    # The empty-relay fixture exits after startup; these assertions are its contract.
    $global:LASTEXITCODE = 0
    Write-Output "FIPS WebRTC startup smoke passed"
}
finally {
    foreach ($name in $savedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], "Process")
    }
    if (Test-Path $smoke) {
        Remove-Item -Recurse -Force $smoke
    }
}
