param(
    [Parameter(Mandatory = $true)]
    [string]$Port,

    [Parameter(Mandatory = $true)]
    [string]$Image
)

$ErrorActionPreference = "Stop"

function Read-Line {
    param([System.IO.Ports.SerialPort]$Serial)
    $line = $Serial.ReadLine()
    Write-Host "< $line"
    return $line.Trim()
}

if (-not (Test-Path $Image)) {
    throw "image not found: $Image"
}

[byte[]]$payload = [System.IO.File]::ReadAllBytes((Resolve-Path $Image))
if (($payload.Length % 4) -ne 0) {
    $pad = 4 - ($payload.Length % 4)
    $padded = New-Object byte[] ($payload.Length + $pad)
    [Array]::Copy($payload, $padded, $payload.Length)
    for ($i = $payload.Length; $i -lt $padded.Length; $i++) {
        $padded[$i] = 0xFF
    }
    $payload = $padded
}

$serial = New-Object System.IO.Ports.SerialPort $Port,115200,None,8,one
$serial.NewLine = "`n"
$serial.ReadTimeout = 8000
$serial.WriteTimeout = 8000
$serial.DtrEnable = $true
$serial.RtsEnable = $true

try {
    $serial.Open()
    Start-Sleep -Milliseconds 500

    $serial.DiscardInBuffer()
    $serial.DiscardOutBuffer()

    Write-Host "> INFO"
    $serial.Write("INFO`n")
    [void](Read-Line $serial)

    Write-Host "> WRITE $($payload.Length)"
    $serial.Write("WRITE $($payload.Length)`n")
    $ready = Read-Line $serial
    if ($ready -ne "READY") {
        throw "device not ready: $ready"
    }

    Write-Host "> binary payload ($($payload.Length) bytes)"
    $serial.BaseStream.Write($payload, 0, $payload.Length)
    $serial.BaseStream.Flush()

    $ok = Read-Line $serial
    if ($ok -ne "OK") {
        throw "image write failed: $ok"
    }

    Write-Host "> BOOT"
    $serial.Write("BOOT`n")
    [void](Read-Line $serial)
    Write-Host "Update command sent. Device will reboot into the new firmware."
}
finally {
    if ($serial.IsOpen) {
        $serial.Close()
    }
}
