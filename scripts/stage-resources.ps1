# Stage the self-contained runtime resources for the Windows desktop bundle:
#   1. the official Node binary (win-x64) under resources/node
#   2. the npm-installed @deepseek-ai/dsh harness under resources/harness
# Run from anywhere; the resource dir is derived from this script's own
# location (scripts/ sits next to src-tauri/).
$ErrorActionPreference = 'Stop'

$NodeVersion = if ($env:NODE_VERSION) { $env:NODE_VERSION } else { 'v22.23.2' }
$DshVersion = if ($env:DSH_VERSION) { $env:DSH_VERSION } else { '0.1.1-rc.2' }
$PnpmVersion = if ($env:PNPM_VERSION) { $env:PNPM_VERSION } else { '11.22.0' }
$Res = Join-Path (Split-Path -Parent $PSScriptRoot) 'src-tauri/resources'

New-Item -ItemType Directory -Force -Path $Res | Out-Null

if (-not (Test-Path (Join-Path $Res 'node/node.exe'))) {
  Write-Host ">> downloading node $NodeVersion (win-x64)"
  $zip = Join-Path $env:TEMP ("node-" + $NodeVersion + "-win-x64.zip")
  Invoke-WebRequest -Uri ("https://nodejs.org/dist/" + $NodeVersion + "/node-" + $NodeVersion + "-win-x64.zip") -OutFile $zip
  $extract = Join-Path $env:TEMP 'node-extract'
  if (Test-Path $extract) { Remove-Item -Recurse -Force $extract }
  Expand-Archive -Path $zip -DestinationPath $extract
  New-Item -ItemType Directory -Force -Path (Join-Path $Res 'node') | Out-Null
  Copy-Item (Join-Path $extract ("node-" + $NodeVersion + "-win-x64/node.exe")) (Join-Path $Res 'node/node.exe')
  # npm/npx shims and their node_modules stay so the bundled toolchain can
  # install plugins standalone; corepack is skipped on purpose.
  Copy-Item (Join-Path $extract ("node-" + $NodeVersion + "-win-x64/npm.cmd")) (Join-Path $Res 'node/npm.cmd')
  Copy-Item (Join-Path $extract ("node-" + $NodeVersion + "-win-x64/npx.cmd")) (Join-Path $Res 'node/npx.cmd')
  Copy-Item -Recurse (Join-Path $extract ("node-" + $NodeVersion + "-win-x64/node_modules")) (Join-Path $Res 'node/node_modules')
  Remove-Item -Recurse -Force $extract
  Remove-Item -Force $zip
}

if (-not (Test-Path (Join-Path $Res 'harness/package.json'))) {
  Write-Host ">> installing @deepseek-ai/dsh@$DshVersion into resources/harness"
  # npm resolving the dsh tree peaks over the default 2GB V8 heap on CI.
  $env:NODE_OPTIONS = "--max-old-space-size=4096"
  & (Join-Path $Res 'node/npm.cmd') install --prefix (Join-Path $Res 'harness') ("@deepseek-ai/dsh@" + $DshVersion) ("pnpm@" + $PnpmVersion) --no-audit --no-fund
  if ($LASTEXITCODE -ne 0) { throw "npm install failed" }
}

$total = (Get-ChildItem -Recurse $Res | Measure-Object -Property Length -Sum).Sum
Write-Host (">> resources staged: {0:N1} MB" -f ($total / 1MB))

