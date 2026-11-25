# Generates the Stream Deck artwork for com.jaygee.audiomixerpro.
# Run from the repository root:  powershell -ExecutionPolicy Bypass -File streamdeck\tools\make-icons.ps1
#
# Everything is drawn at 72 px and again at 144 px (@2x), because the Stream
# Deck app picks the @2x file automatically when one exists.

Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$imgs = Join-Path $root "com.jaygee.audiomixerpro.sdPlugin\imgs"
New-Item -ItemType Directory -Force -Path $imgs | Out-Null

$bg = [System.Drawing.Color]::FromArgb(26, 30, 37)
$dim = [System.Drawing.Color]::FromArgb(124, 134, 150)
$amber = [System.Drawing.Color]::FromArgb(224, 158, 55)

function New-Tile([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap $size, $size
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.Clear($bg)
    return @($bmp, $g)
}

# One speaker with two sound arcs. 72 px design, scaled.
function Draw-Speaker($g, [int]$size, $color, [bool]$slash) {
    $s = $size / 72.0
    $body = New-Object System.Drawing.Drawing2D.GraphicsPath
    $pts = @(
        (New-Object System.Drawing.PointF (18 * $s), (28 * $s)),
        (New-Object System.Drawing.PointF (26 * $s), (28 * $s)),
        (New-Object System.Drawing.PointF (36 * $s), (18 * $s)),
        (New-Object System.Drawing.PointF (36 * $s), (54 * $s)),
        (New-Object System.Drawing.PointF (26 * $s), (44 * $s)),
        (New-Object System.Drawing.PointF (18 * $s), (44 * $s))
    )
    $body.AddPolygon($pts)
    $brush = New-Object System.Drawing.SolidBrush $color
    $g.FillPath($brush, $body)
    $pen = New-Object System.Drawing.Pen $color, (2.6 * $s)
    $g.DrawArc($pen, (38 * $s), (26 * $s), (14 * $s), (20 * $s), -55, 110)
    $g.DrawArc($pen, (38 * $s), (18 * $s), (26 * $s), (36 * $s), -52, 104)
    if ($slash) {
        $thick = New-Object System.Drawing.Pen $amber, (4.5 * $s)
        $g.DrawLine($thick, (14 * $s), (58 * $s), (58 * $s), (14 * $s))
        $thick.Dispose()
    }
    $brush.Dispose()
    $pen.Dispose()
    $body.Dispose()
}

# Three faders, for "all strips".
function Draw-Faders($g, [int]$size, $color, [bool]$slash) {
    $s = $size / 72.0
    $pen = New-Object System.Drawing.Pen $color, (2.4 * $s)
    $knob = New-Object System.Drawing.SolidBrush $color
    foreach ($x in @(22, 36, 50)) {
        $g.DrawLine($pen, ($x * $s), (16 * $s), ($x * $s), (56 * $s))
    }
    $heights = @(30, 22, 40)
    for ($i = 0; $i -lt 3; $i++) {
        $x = @(22, 36, 50)[$i] * $s
        $y = $heights[$i] * $s
        $g.FillRectangle($knob, ($x - (5 * $s)), ($y - (3 * $s)), (10 * $s), (6 * $s))
    }
    if ($slash) {
        $thick = New-Object System.Drawing.Pen $amber, (4.5 * $s)
        $g.DrawLine($thick, (12 * $s), (60 * $s), (60 * $s), (12 * $s))
        $thick.Dispose()
    }
    $pen.Dispose()
    $knob.Dispose()
}

function Save-Tile([string]$name, [string]$draw, $color, [bool]$slash) {
    foreach ($size in @(72, 144)) {
        $pair = New-Tile $size
        $bmp = $pair[0]
        $g = $pair[1]
        if ($draw -eq "speaker") {
            Draw-Speaker $g $size $color $slash
        } else {
            Draw-Faders $g $size $color $slash
        }
        $file = if ($size -eq 72) { "$name.png" } else { "$name@2x.png" }
        $bmp.Save((Join-Path $imgs $file), [System.Drawing.Imaging.ImageFormat]::Png)
        $g.Dispose()
        $bmp.Dispose()
    }
    Write-Host "wrote $name.png / $name@2x.png"
}

Save-Tile "mute-live" "speaker" $dim $false
Save-Tile "mute-muted" "speaker" $amber $true
Save-Tile "mute-all-live" "faders" $dim $false
Save-Tile "mute-all-muted" "faders" $amber $true
Save-Tile "plugin" "faders" $amber $false
Save-Tile "category" "faders" $amber $false
