# Regenerates assets/icon.ico (and icon.png preview) - Windows PowerShell 5.1, System.Drawing only.
#   powershell -ExecutionPolicy Bypass -File assets\make_icon.ps1
Add-Type -AssemblyName System.Drawing

$sizes = 16, 24, 32, 48, 64, 128, 256
$here = Split-Path -Parent $MyInvocation.MyCommand.Path

function New-IconPng([int]$s) {
    $bmp = New-Object System.Drawing.Bitmap $s, $s, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.Clear([System.Drawing.Color]::Transparent)

    # Rounded tile with a subtle vertical gradient.
    $m = [Math]::Max(0.5, $s * 0.03)
    $r = $s * 0.22
    $d = $r * 2
    $w = $s - 2 * $m
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc($m, $m, $d, $d, 180, 90)
    $path.AddArc($m + $w - $d, $m, $d, $d, 270, 90)
    $path.AddArc($m + $w - $d, $m + $w - $d, $d, $d, 0, 90)
    $path.AddArc($m, $m + $w - $d, $d, $d, 90, 90)
    $path.CloseFigure()
    $rect = New-Object System.Drawing.RectangleF 0, 0, $s, $s
    $grad = New-Object System.Drawing.Drawing2D.LinearGradientBrush $rect,
        ([System.Drawing.Color]::FromArgb(255, 40, 44, 60)),
        ([System.Drawing.Color]::FromArgb(255, 20, 22, 31)), 90
    $g.FillPath($grad, $path)
    $edge = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(255, 66, 72, 94)), ([Math]::Max(1, $s * 0.02))
    $g.DrawPath($edge, $path)

    # Faint baseline (skipped at tiny sizes, it would just be noise).
    if ($s -ge 32) {
        $base = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(45, 255, 255, 255)), ([Math]::Max(1, $s * 0.015))
        $g.DrawLine($base, $s * 0.16, $s * 0.80, $s * 0.84, $s * 0.80)
    }

    # Frametime-style line in the overlay's "good FPS" green.
    $pts = @(
        @(0.15, 0.66), @(0.29, 0.50), @(0.42, 0.60), @(0.58, 0.24), @(0.72, 0.42), @(0.86, 0.30)
    ) | ForEach-Object { New-Object System.Drawing.PointF ([single]($_[0] * $s)), ([single]($_[1] * $s)) }
    $lw = [Math]::Max(2, $s * 0.085)
    $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(255, 110, 231, 140)), $lw
    $pen.LineJoin = [System.Drawing.Drawing2D.LineJoin]::Round
    $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
    if ($s -ge 48) {
        $glow = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(50, 110, 231, 140)), ($lw * 2.2)
        $glow.LineJoin = [System.Drawing.Drawing2D.LineJoin]::Round
        $glow.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
        $glow.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
        $g.DrawLines($glow, $pts)
    }
    $g.DrawLines($pen, $pts)
    $g.Dispose()

    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    if ($s -eq 256) { $bmp.Save((Join-Path $here 'icon.png'), [System.Drawing.Imaging.ImageFormat]::Png) }
    $bmp.Dispose()
    return , $ms.ToArray()
}

$images = @()
foreach ($s in $sizes) { $images += , (New-IconPng $s) }

# ICO container with PNG-compressed entries.
$out = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter $out
$bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]
    $dim = if ($s -ge 256) { 0 } else { $s }
    $bw.Write([byte]$dim); $bw.Write([byte]$dim); $bw.Write([byte]0); $bw.Write([byte]0)
    $bw.Write([uint16]1); $bw.Write([uint16]32)
    $bw.Write([uint32]$images[$i].Length); $bw.Write([uint32]$offset)
    $offset += $images[$i].Length
}
foreach ($img in $images) { $bw.Write($img) }
$bw.Flush()
[System.IO.File]::WriteAllBytes((Join-Path $here 'icon.ico'), $out.ToArray())
Write-Host "wrote icon.ico ($($out.Length) bytes) and icon.png"
