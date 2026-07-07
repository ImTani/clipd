# T0 encoder rate-control PROBE — reproducible evidence for the §6.1 amendment.
# See DECISIONS.md "2026-07-07 — T0 resolution" and M7-M8-PLAN.md §1.
#
# The quality sweep proved AVEncCommonQuality is a no-op on the NVENC MFT and that
# MF_MT_AVG_BITRATE alone controls output. This probe settled the "probe CQP, auto-
# fall-back" decision: test every viable rate-control mode (does ANY quality/QP knob
# move bitrate? is true CQP via AVEncVideoEncodeQP even accepted?), and if not, measure
# which VBR bitrate-target config is content-adaptive (mandelbrot >> testsrc2). Result:
# no quality/QP lever exists -> shipping encoder targets a bitrate via PeakConstrainedVBR.
#
# Drives deterministic ffmpeg lavfi content in a borderless WINDOW (NOT exclusive -fs,
# which starves WGC monitor capture and hangs the encoder) and captures it via the
# hidden `record --encode-*` calibration hooks. Hard per-run timeout force-kills clipd
# so nothing can hang. Fully unattended; covers the primary display for ~4 min.
#
# Requires: ffmpeg/ffplay/ffprobe on PATH; a hardware H.264 encoder. Windows-only.
# Usage (from anywhere):  powershell -ExecutionPolicy Bypass -File tools\calibration\t0_sweep.ps1
[CmdletBinding()]
param(
  [int]$RecSecs = 15,
  [string]$OutDir = "$env:TEMP\clipd_t0out"
)

$ErrorActionPreference = 'Stop'
# Machine-specific cargo bin (the Nitro test box keeps X:\cargo off PATH); harmless
# elsewhere — only prepended when present.
if (Test-Path 'X:\cargo\bin') { $env:Path = "X:\cargo\bin;$env:Path" }
$env:RUST_LOG = 'info'
# Repo root = two levels up from this script (tools/calibration/).
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$exe  = "$repo\target\release\clipd.exe"
$W = 1920; $H = 1080
$RunTimeoutMs = ($RecSecs + 20) * 1000

$lavfi = @{
  mandelbrot = "mandelbrot=size=${W}x${H}:rate=30"   # pathological high-entropy
  testsrc2   = "testsrc2=size=${W}x${H}:rate=60"      # moderate motion
}

# Declarative run matrix. Each run: a rate-control mode + optional quality/qp/avg/max.
# Group 1-2: sweep a knob at two extremes on mandelbrot -> flat bitrate = dead knob.
# Group 3: 16 Mbps average target across two sources -> adaptivity = mandelbrot>>testsrc2.
$runs = @(
  @{ id='C1a'; note='quality-mode, quality knob lo'; src='mandelbrot'; rc='quality'; q=50 }
  @{ id='C1b'; note='quality-mode, quality knob hi'; src='mandelbrot'; rc='quality'; q=95 }
  @{ id='C2a'; note='UVBR, quality knob lo';         src='mandelbrot'; rc='uvbr';    q=50 }
  @{ id='C2b'; note='UVBR, quality knob hi';         src='mandelbrot'; rc='uvbr';    q=95 }
  @{ id='C3a'; note='quality-mode, CQP QP=18';       src='mandelbrot'; rc='quality'; qp=18 }
  @{ id='C3b'; note='quality-mode, CQP QP=30';       src='mandelbrot'; rc='quality'; qp=30 }
  @{ id='C4a'; note='UVBR, CQP QP=18';               src='mandelbrot'; rc='uvbr';    qp=18 }
  @{ id='C4b'; note='UVBR, CQP QP=30';               src='mandelbrot'; rc='uvbr';    qp=30 }
  @{ id='C5a'; note='UVBR avg=16M (hard)';           src='mandelbrot'; rc='uvbr';    avg=16000000 }
  @{ id='C5b'; note='UVBR avg=16M (moderate)';       src='testsrc2';   rc='uvbr';    avg=16000000 }
  @{ id='C6a'; note='PCVBR avg=16M max=24M (hard)';  src='mandelbrot'; rc='pcvbr';   avg=16000000; max=24000000 }
  @{ id='C6b'; note='PCVBR avg=16M max=24M (mod)';   src='testsrc2';   rc='pcvbr';   avg=16000000; max=24000000 }
  @{ id='C7';  note='CBR avg=16M reference (hard)';  src='mandelbrot'; rc='cbr';     avg=16000000 }
)

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
Write-Host "=== T0 rate-control probe: building release ===" -ForegroundColor Cyan
Push-Location $repo; cargo build --release --locked --quiet; Pop-Location
if (-not (Test-Path $exe)) { throw "release exe not found at $exe" }

function Stop-Clipd  { Get-Process -Name clipd  -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue }
function Stop-Ffplay { Get-Process -Name ffplay -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue }

function Invoke-Run {
  param([hashtable]$Spec)
  $OutFile = "$OutDir\$($Spec.id).mp4"
  Remove-Item $OutFile -Force -ErrorAction SilentlyContinue
  Stop-Clipd; Stop-Ffplay
  Start-Process ffplay -ArgumentList @(
    '-f','lavfi','-i',$lavfi[$Spec.src],'-x',"$W",'-y',"$H",'-noborder','-an','-loglevel','quiet'
  ) | Out-Null
  Start-Sleep -Seconds 2

  $recArgs = @('record','--seconds',"$RecSecs",'--out',$OutFile,'--encode-rc-mode',$Spec.rc)
  if ($Spec.ContainsKey('q'))   { $recArgs += @('--encode-quality',"$($Spec.q)") }
  if ($Spec.ContainsKey('qp'))  { $recArgs += @('--encode-qp',"$($Spec.qp)") }
  if ($Spec.ContainsKey('avg')) { $recArgs += @('--encode-avg-bitrate',"$($Spec.avg)") }
  if ($Spec.ContainsKey('max')) { $recArgs += @('--encode-max-bitrate',"$($Spec.max)") }
  $outLog = "$OutFile.out"; $errLog = "$OutFile.err"
  $p = Start-Process $exe -PassThru -NoNewWindow -RedirectStandardOutput $outLog -RedirectStandardError $errLog -ArgumentList $recArgs
  $exited = $p.WaitForExit($RunTimeoutMs)
  if (-not $exited) { try { $p.Kill() } catch {}; $hung = $true } else { $hung = $false }
  Stop-Ffplay; Stop-Clipd

  $logTxt = (Get-Content $outLog,$errLog -ErrorAction SilentlyContinue) -replace '\x1b\[[0-9;]*m',''
  $qpm = $logTxt | Select-String -Pattern 'qp_status="?([a-z]+)"?' | Select-Object -First 1
  $qpStatus = if ($qpm) { $qpm.Matches.Groups[1].Value } else { 'n/a' }

  if ($hung -or -not (Test-Path $OutFile)) {
    $fail = if ($hung) { 'HANG' } else { 'no-file' }
    return [pscustomobject]@{ id=$Spec.id; source=$Spec.src; rc=$Spec.rc; knob=(Knob $Spec); qp_status=$qpStatus; stream_mbps=$fail; derived_mbps='-' }
  }
  $sb  = ffprobe -v error -select_streams v:0 -show_entries stream=bit_rate -of csv=p=0 $OutFile
  $dur = ffprobe -v error -show_entries format=duration -of csv=p=0 $OutFile
  $sz  = (Get-Item $OutFile).Length
  $streamMbps  = if ($sb  -match '^\d+$')          { [math]::Round([double]$sb/1e6,2) } else { $null }
  $derivedMbps = if ($dur -and [double]$dur -gt 0) { [math]::Round(($sz*8)/([double]$dur*1e6),2) } else { $null }
  [pscustomobject]@{ id=$Spec.id; source=$Spec.src; rc=$Spec.rc; knob=(Knob $Spec); qp_status=$qpStatus; stream_mbps=$streamMbps; derived_mbps=$derivedMbps }
}

function Knob([hashtable]$s) {
  $parts = @()
  if ($s.ContainsKey('q'))   { $parts += "q=$($s.q)" }
  if ($s.ContainsKey('qp'))  { $parts += "qp=$($s.qp)" }
  if ($s.ContainsKey('avg')) { $parts += "avg=$([int]($s.avg/1e6))M" }
  if ($s.ContainsKey('max')) { $parts += "max=$([int]($s.max/1e6))M" }
  ($parts -join ',')
}

Stop-Clipd; Stop-Ffplay
$rows = @()
try {
  foreach ($spec in $runs) {
    Write-Host ("run {0,-4} {1,-32} ..." -f $spec.id,$spec.note) -NoNewline -ForegroundColor Yellow
    $r = Invoke-Run $spec
    Write-Host (" rc={0,-7} {1,-14} stream={2} Mbps  qp={3}" -f $r.rc,$r.knob,$r.stream_mbps,$r.qp_status) -ForegroundColor Green
    $rows += $r
  }
}
finally { Stop-Clipd; Stop-Ffplay }

$csv = "$OutDir\probe_results.csv"
$rows | Export-Csv -NoTypeInformation -Path $csv
Write-Host "`n=== PROBE RESULTS ===" -ForegroundColor Cyan
$rows | Format-Table id,source,rc,knob,qp_status,stream_mbps,derived_mbps -AutoSize | Out-String | Write-Host
Write-Host "csv: $csv"
Write-Host "read: C1-C4 = does a quality/QP knob move bitrate? (flat=dead; qp_status=accepted means true CQP works)"
Write-Host "      C5-C6 = adaptivity: mandelbrot >> testsrc2 means VBR is content-adaptive; ~equal means CBR-like"