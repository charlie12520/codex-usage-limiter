[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [ValidatePattern('^v?\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$')]
    [string]$Version,

    [switch]$Silent,

    [switch]$DownloadOnly,

    [string]$Destination
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repository = 'charlie12520/codex-usage-limiter'
$installerNamePattern = '^codex-usage-limiter-v.+-windows-x64-setup\.exe$'
$requestHeaders = @{
    Accept                 = 'application/vnd.github+json'
    'X-GitHub-Api-Version' = '2022-11-28'
    'User-Agent'           = 'codex-usage-limiter-installer'
}
if ($env:GITHUB_TOKEN) {
    $requestHeaders.Authorization = "Bearer $env:GITHUB_TOKEN"
}

function Invoke-GitHubRestMethod {
    param([Parameter(Mandatory = $true)][string]$Uri)

    $requestParameters = @{
        Uri         = $Uri
        Headers     = $requestHeaders
        ErrorAction = 'Stop'
    }
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        $requestParameters.UseBasicParsing = $true
    }

    Invoke-RestMethod @requestParameters
}

function Save-GitHubAsset {
    param(
        [Parameter(Mandatory = $true)][string]$Uri,
        [Parameter(Mandatory = $true)][string]$Path
    )

    $requestParameters = @{
        Uri         = $Uri
        Headers     = $requestHeaders
        OutFile     = $Path
        ErrorAction = 'Stop'
    }
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        $requestParameters.UseBasicParsing = $true
    }

    Invoke-WebRequest @requestParameters
}

function Confirm-AssetDigest {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Digest
    )

    if ($Digest -notmatch '^sha256:([0-9a-fA-F]{64})$') {
        throw 'GitHub did not provide a usable SHA-256 digest for the installer.'
    }

    $expectedHash = $Matches[1].ToUpperInvariant()
    $actualHash = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToUpperInvariant()
    if ($actualHash -ne $expectedHash) {
        throw "Installer digest mismatch. Expected $expectedHash but downloaded $actualHash."
    }

    $actualHash
}

if ($PSVersionTable.PSVersion -lt [version]'5.1') {
    throw 'Codex Usage Limiter installation requires PowerShell 5.1 or newer.'
}
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'This installer is for Windows only.'
}
if ($Destination -and -not $DownloadOnly) {
    throw '-Destination can only be used with -DownloadOnly.'
}

if ($PSVersionTable.PSVersion.Major -lt 6) {
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
}

$releaseApiUri = if ($Version) {
    $releaseTag = if ($Version.StartsWith('v')) { $Version } else { "v$Version" }
    $encodedTag = [Uri]::EscapeDataString($releaseTag)
    "https://api.github.com/repos/$repository/releases/tags/$encodedTag"
} else {
    "https://api.github.com/repos/$repository/releases/latest"
}

Write-Host 'Finding the Codex Usage Limiter release...'
$release = Invoke-GitHubRestMethod -Uri $releaseApiUri
$installerAssets = @($release.assets | Where-Object { $_.name -match $installerNamePattern })
if ($installerAssets.Count -ne 1) {
    throw "Expected one Windows setup executable in release $($release.tag_name), found $($installerAssets.Count)."
}

$installerAsset = $installerAssets[0]
if (-not ($installerAsset.PSObject.Properties.Name -contains 'digest')) {
    throw 'GitHub did not return a digest for the installer asset.'
}

if ($DownloadOnly) {
    $downloadPath = if ($Destination) {
        [IO.Path]::GetFullPath($Destination)
    } else {
        Join-Path -Path (Get-Location).Path -ChildPath $installerAsset.name
    }
    if (Test-Path -LiteralPath $downloadPath) {
        throw "Destination already exists: $downloadPath"
    }
    if (-not $PSCmdlet.ShouldProcess($downloadPath, "download and verify $($installerAsset.name)")) {
        return
    }

    try {
        Write-Host "Downloading $($installerAsset.name)..."
        Save-GitHubAsset -Uri $installerAsset.browser_download_url -Path $downloadPath
        $verifiedHash = Confirm-AssetDigest -Path $downloadPath -Digest $installerAsset.digest
    } catch {
        Remove-Item -LiteralPath $downloadPath -Force -ErrorAction SilentlyContinue
        throw
    }

    [pscustomobject]@{
        Release = $release.tag_name
        Path    = $downloadPath
        SHA256  = $verifiedHash
    }
    return
}

$installAction = if ($Silent) { 'download, verify, and install silently' } else { 'download, verify, and open the installer' }
if (-not $PSCmdlet.ShouldProcess("Codex Usage Limiter $($release.tag_name)", $installAction)) {
    return
}

$temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
$installTempDir = Join-Path $temporaryRoot ("codex-usage-limiter-install-" + [Guid]::NewGuid().ToString('N'))
$downloadPath = Join-Path $installTempDir $installerAsset.name

try {
    New-Item -ItemType Directory -Path $installTempDir | Out-Null
    Write-Host "Downloading $($installerAsset.name)..."
    Save-GitHubAsset -Uri $installerAsset.browser_download_url -Path $downloadPath
    $null = Confirm-AssetDigest -Path $downloadPath -Digest $installerAsset.digest
    Write-Host 'SHA-256 verified.'

    $startParameters = @{
        FilePath     = $downloadPath
        Wait         = $true
        PassThru     = $true
        ErrorAction  = 'Stop'
    }
    if ($Silent) {
        $startParameters.ArgumentList = '/S'
    }

    $installerProcess = Start-Process @startParameters
    if ($installerProcess.ExitCode -ne 0) {
        throw "The installer exited with code $($installerProcess.ExitCode)."
    }

    Write-Host "Codex Usage Limiter $($release.tag_name) installation finished."
} finally {
    $resolvedTempDir = [IO.Path]::GetFullPath($installTempDir)
    $isContainedTempPath = $resolvedTempDir.StartsWith(
        $temporaryRoot + [IO.Path]::DirectorySeparatorChar,
        [StringComparison]::OrdinalIgnoreCase
    )
    if ($isContainedTempPath -and (Test-Path -LiteralPath $resolvedTempDir)) {
        Remove-Item -LiteralPath $resolvedTempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
