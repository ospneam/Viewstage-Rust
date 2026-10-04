$file = 'D:\code\Viewstage-dev\src\views\oobe.rs'
$content = Get-Content $file -Raw

# Fix 1: step_theme - inline theme_row
$content = $content -replace "let theme_row: Element.*? = row!\[dark_btn, light_btn\].spacing\(12\).into\(\);\s+theme_row,", "row![dark_btn, light_btn].spacing(12).into(),"

# Fix 2: step_performance - inline frame_row
$content = $content -replace "let frame_row: Element.*? = row!\[low_btn, adaptive_btn, high_btn\].spacing\(6\).into\(\);\s+frame_row,", "row![low_btn, adaptive_btn, high_btn].spacing(6).into(),"

# Fix 3: step_performance - inline pen_row
$content = $content -replace "let pen_row: Element.*? = row!\[off_btn, limited_btn, full_btn\].spacing\(6\).into\(\);\s+pen_row,", "row![off_btn, limited_btn, full_btn].spacing(6).into(),"

# Fix 4: step_camera - inline res_pick
$content = $content -replace "let res_pick: iced::Element.*? = pick_list\([\s\S]*?\).into\(\);\s+res_pick,", "pick_list(resolutions, Some(Resolution(app.config.source.camera_width, app.config.source.camera_height)), move |value| { Message::OobeAction(OobeAction::CameraResolutionSelected((value.0, value.1))) }).into(),"

Set-Content -Path $file -Value $content -Encoding UTF8
Write-Output "Done"