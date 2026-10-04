$file = 'D:\code\Viewstage-dev\src\views\oobe.rs'
$lines = Get-Content $file
$newLines = @()
$skip = $false
for ($i = 0; $i -lt $lines.Count; $i++) {
    $line = $lines[$i]
    if ($line -match '^\s+let res_pick: iced::Element') {
        $skip = $true
        continue
    }
    if ($skip) {
        if ($line -match '^\s+res_pick,') {
            $skip = $false
            continue
        }
        # Check if we're past the closing ).into() line
        if ($line -match '^\s+\)\s*$' -or $line -match '^\s+\)\s*\.into\(\)') {
            # Still inside the pick_list call, keep skipping
        }
        continue
    }
    $newLines += $line
}
Set-Content -Path $file -Value ($newLines -join "`r`n") -Encoding UTF8
Write-Output "Done"