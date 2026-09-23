# Generate a multi-size Windows ICO from a square PNG source.
# Usage: powershell -File make-ico.ps1 <source.png> <out.ico>
param(
    [Parameter(Mandatory=$true)][string]$Source,
    [Parameter(Mandatory=$true)][string]$Output
)
Add-Type -AssemblyName System.Drawing

$src = [System.Drawing.Image]::FromFile($Source)

function New-SizedBitmap {
    param([System.Drawing.Image]$img, [int]$size)
    $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
    $g.DrawImage($img, 0, 0, $size, $size)
    $g.Dispose()
    return $bmp
}

function Get-PngBytes {
    param([System.Drawing.Image]$img)
    $ms = New-Object System.IO.MemoryStream
    $img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    return $ms.ToArray()
}

function New-BmpEntry {
    param([System.Drawing.Bitmap]$bmp)
    $s = $bmp.Width
    $rect = New-Object System.Drawing.Rectangle(0, 0, $s, $s)
    $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $stride = $data.Stride
    $pixels = New-Object byte[] ($stride * $s)
    [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $pixels, 0, $pixels.Length)
    $bmp.UnlockBits($data)

    $ms = New-Object System.IO.MemoryStream
    $bw = New-Object System.IO.BinaryWriter($ms)

    # BITMAPINFOHEADER (40 bytes)
    $bw.Write([UInt32]40)
    $bw.Write([Int32]$s)
    $bw.Write([Int32]($s * 2))
    $bw.Write([UInt16]1)
    $bw.Write([UInt16]32)
    $bw.Write([UInt32]0)
    $bw.Write([UInt32]($s * $s * 4))
    $bw.Write([Int32]0)
    $bw.Write([Int32]0)
    $bw.Write([UInt32]0)
    $bw.Write([UInt32]0)

    # Pixel rows bottom-up, BGRA
    for ($y = $s - 1; $y -ge 0; $y--) {
        $rowStart = $y * $stride
        for ($x = 0; $x -lt $s; $x++) {
            $o = $rowStart + $x * 4
            $bw.Write([byte]$pixels[$o])
            $bw.Write([byte]$pixels[$o + 1])
            $bw.Write([byte]$pixels[$o + 2])
            $bw.Write([byte]$pixels[$o + 3])
        }
    }
    # AND mask (1bpp, all zero; transparency comes from alpha)
    $maskRow = [int][Math]::Ceiling($s / 32.0) * 4
    $mask = New-Object byte[] ($maskRow * $s)
    $bw.Write($mask)
    $bw.Flush()
    return $ms.ToArray()
}

$entries = New-Object 'System.Collections.Generic.List[byte[]]'
foreach ($size in @(16, 32, 48, 64)) {
    $bmp = New-SizedBitmap $src $size
    $entries.Add((New-BmpEntry $bmp))
    $bmp.Dispose()
}
$bmp256 = New-SizedBitmap $src 256
$png256 = Get-PngBytes $bmp256
$bmp256.Dispose()
$entries.Add($png256)

$sizes = @(16, 32, 48, 64, 256)
$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($ms)
$bw.Write([UInt16]0)
$bw.Write([UInt16]1)
$bw.Write([UInt16]$entries.Count)
$offset = 6 + 16 * $entries.Count
for ($i = 0; $i -lt $entries.Count; $i++) {
    $size = $sizes[$i]
    if ($size -ge 256) { $dim = [byte]0 } else { $dim = [byte]$size }
    $bw.Write([byte]$dim)
    $bw.Write([byte]$dim)
    $bw.Write([byte]0)
    $bw.Write([byte]0)
    $bw.Write([UInt16]1)
    $bw.Write([UInt16]32)
    $bw.Write([UInt32]$entries[$i].Length)
    $bw.Write([UInt32]$offset)
    $offset += $entries[$i].Length
}
foreach ($e in $entries) { $bw.Write($e) }
$bw.Flush()
[System.IO.File]::WriteAllBytes($Output, $ms.ToArray())
$src.Dispose()
Write-Output ("ICO written: {0} ({1} bytes)" -f $Output, ([System.IO.FileInfo]::new($Output).Length))
