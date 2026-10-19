# 截取整个屏幕的脚本
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

# 获取主屏幕信息
$screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$width = $screen.Width
$height = $screen.Height

# 创建位图
$bitmap = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)

# 截取屏幕
$graphics.CopyFromScreen($screen.Location, [System.Drawing.Point]::Empty, $screen.Size)

# 保存
$outputPath = "D:\Project\quota-bar\demo_screenshots\screen_$(Get-Date -Format 'yyyyMMdd_HHmmss').png"
$bitmap.Save($outputPath, [System.Drawing.Imaging.ImageFormat]::Png)

# 清理
$graphics.Dispose()
$bitmap.Dispose()

Write-Host "Screenshot saved to: $outputPath"
