param([string[]]$Steps, [int]$ProcessId = 0)
# Drives only a test build: pass -ProcessId, or exactly one rustcommander.exe
# started from this repo (target\...) must be running. The user's installed
# copy is never touched.
# Steps: keys:<SendKeys> | wait:<ms> | shot:<name> | click:x,y | sclick:x,y (Shift+click) | mclick:x,y (middle) | dclick:x,y | rclick:x,y | wheel:x,y,notches[,ctrl]
#        drag:x1,y1,x2,y2[,shift] + drop | rdrag:x1,y1,x2,y2 + rdrop (right button)
# Coordinates are relative to the window's top-left corner (as in screenshots).
Add-Type @"
using System; using System.Runtime.InteropServices;
public class W4 {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, IntPtr pid);
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool attach);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int x, int y, uint d, UIntPtr e);
  [DllImport("user32.dll")] public static extern void keybd_event(byte k, byte s, uint f, UIntPtr e);
  public struct RECT { public int L,T,R,B; }
}
"@
Add-Type -AssemblyName System.Drawing
$scratch = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = (Resolve-Path (Join-Path $scratch '..')).Path.TrimEnd('\') + '\'
if ($ProcessId) { $p = Get-Process -Id $ProcessId -ErrorAction Stop }
else {
  $p = @(Get-Process rustcommander -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($repo, 'OrdinalIgnoreCase') })
  if ($p.Count -ne 1) { throw "Expected exactly one rustcommander.exe under $repo, found $($p.Count). Pass -ProcessId." }
  $p = $p[0]
}
if (-not $p.Path -or -not $p.Path.StartsWith($repo, 'OrdinalIgnoreCase')) { throw "PID $($p.Id) ($($p.Path)) is not a build from this repo; refusing to drive it." }
# The window starts hidden until it's placed: wait for it to get a handle.
$deadline = (Get-Date).AddSeconds(10)
while ($p.MainWindowHandle -eq [IntPtr]::Zero) {
  if ((Get-Date) -gt $deadline) { throw "PID $($p.Id) has no visible window after 10 s." }
  Start-Sleep -Milliseconds 200; $p.Refresh()
}
$h = $p.MainWindowHandle
# Windows' foreground lock refuses SetForegroundWindow from a background
# process (it only flashes the taskbar button). Try plain, then attached to
# the foreground thread's input, then after a synthetic Alt (which makes us
# the sender of the last input). Verify, so keys never go to another window.
function Focus-Window([IntPtr]$h) {
  if ([W4]::IsIconic($h)) { [W4]::ShowWindow($h, 9) | Out-Null }  # SW_RESTORE
  for ($try = 0; $try -lt 3; $try++) {
    switch ($try) {
      0 { [W4]::SetForegroundWindow($h) | Out-Null }
      1 {
        $fg = [W4]::GetWindowThreadProcessId([W4]::GetForegroundWindow(), [IntPtr]::Zero)
        $me = [W4]::GetCurrentThreadId()
        [W4]::AttachThreadInput($me, $fg, $true) | Out-Null
        [W4]::BringWindowToTop($h) | Out-Null
        [W4]::SetForegroundWindow($h) | Out-Null
        [W4]::AttachThreadInput($me, $fg, $false) | Out-Null
      }
      2 {
        [W4]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero)  # Alt down
        [W4]::SetForegroundWindow($h) | Out-Null
        [W4]::keybd_event(0x12, 0, 2, [UIntPtr]::Zero)  # Alt up
      }
    }
    Start-Sleep -Milliseconds 300
    if ([W4]::GetForegroundWindow() -eq $h) { return }
  }
  throw "Could not bring PID $($p.Id) to the foreground; not sending input to another window."
}
Focus-Window $h
Start-Sleep -Milliseconds 100
$r = New-Object W4+RECT; [W4]::GetWindowRect($h, [ref]$r) | Out-Null
$ws = New-Object -ComObject WScript.Shell
function MoveTo($x, $y) { [W4]::SetCursorPos($r.L + $x, $r.T + $y) | Out-Null; Start-Sleep -Milliseconds 60 }
foreach ($s in $Steps) {
  $kind, $arg = $s -split ':', 2
  switch ($kind) {
    'keys' { $ws.SendKeys($arg); Start-Sleep -Milliseconds 300 }
    'wait' { Start-Sleep -Milliseconds ([int]$arg) }
    'click' { $x, $y = $arg -split ','; MoveTo $x $y; [W4]::mouse_event(2,0,0,0,[UIntPtr]::Zero); [W4]::mouse_event(4,0,0,0,[UIntPtr]::Zero); Start-Sleep -Milliseconds 300 }
    'sclick' { $x, $y = $arg -split ','; MoveTo $x $y; [W4]::keybd_event(0x10,0,0,[UIntPtr]::Zero); Start-Sleep -Milliseconds 100; [W4]::mouse_event(2,0,0,0,[UIntPtr]::Zero); [W4]::mouse_event(4,0,0,0,[UIntPtr]::Zero); Start-Sleep -Milliseconds 100; [W4]::keybd_event(0x10,0,2,[UIntPtr]::Zero); Start-Sleep -Milliseconds 300 }
    'mclick' { $x, $y = $arg -split ','; MoveTo $x $y; [W4]::mouse_event(0x20,0,0,0,[UIntPtr]::Zero); [W4]::mouse_event(0x40,0,0,0,[UIntPtr]::Zero); Start-Sleep -Milliseconds 300 }
    'dclick' { $x, $y = $arg -split ','; MoveTo $x $y; for ($i = 0; $i -lt 2; $i++) { [W4]::mouse_event(2,0,0,0,[UIntPtr]::Zero); [W4]::mouse_event(4,0,0,0,[UIntPtr]::Zero); Start-Sleep -Milliseconds 80 }; Start-Sleep -Milliseconds 300 }
    'wheel' {
      # wheel:x,y,notches[,ctrl]
      $x, $y, $n, $mod = $arg -split ','; MoveTo $x $y
      if ($mod -eq 'ctrl') { [W4]::keybd_event(0x11,0,0,[UIntPtr]::Zero); Start-Sleep -Milliseconds 100 }
      [W4]::mouse_event(0x800,0,0,[BitConverter]::ToUInt32([BitConverter]::GetBytes([int]$n * 120), 0),[UIntPtr]::Zero); Start-Sleep -Milliseconds 300
      if ($mod -eq 'ctrl') { [W4]::keybd_event(0x11,0,2,[UIntPtr]::Zero); Start-Sleep -Milliseconds 100 }
    }
    'rclick' { $x, $y = $arg -split ','; MoveTo $x $y; [W4]::mouse_event(8,0,0,0,[UIntPtr]::Zero); [W4]::mouse_event(16,0,0,0,[UIntPtr]::Zero); Start-Sleep -Milliseconds 500 }
    { $_ -in 'drag','rdrag' } {
      # drag:x1,y1,x2,y2[,x3,y3...][,shift] - press at the first point, move through the rest
      $a = $arg -split ','; $shift = $a[-1] -eq 'shift'; if ($shift) { $a = $a[0..($a.Count-2)] }
      MoveTo $a[0] $a[1]; [W4]::mouse_event($(if ($kind -eq 'rdrag') { 8 } else { 2 }),0,0,0,[UIntPtr]::Zero)
      if ($shift) { [W4]::keybd_event(0x10,0,0,[UIntPtr]::Zero) }
      for ($k = 2; $k -lt $a.Count; $k += 2) {
        $x0 = [int]$a[$k-2]; $y0 = [int]$a[$k-1]; $x1 = [int]$a[$k]; $y1 = [int]$a[$k+1]
        for ($i = 1; $i -le 12; $i++) { MoveTo ($x0 + ($x1-$x0)*$i/12) ($y0 + ($y1-$y0)*$i/12); Start-Sleep -Milliseconds 30 }
      }
      Start-Sleep -Milliseconds 200
    }
    'rdrop' { [W4]::mouse_event(16,0,0,0,[UIntPtr]::Zero); Start-Sleep -Milliseconds 800 }
    'drop' { [W4]::mouse_event(4,0,0,0,[UIntPtr]::Zero); [W4]::keybd_event(0x10,0,2,[UIntPtr]::Zero); Start-Sleep -Milliseconds 400 }
    'shot' {
      [W4]::GetWindowRect($h, [ref]$r) | Out-Null
      $bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
      $g = [System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($r.L, $r.T, 0, 0, $bmp.Size)
      $bmp.Save((Join-Path $scratch "$arg.png")); $g.Dispose(); $bmp.Dispose()
    }
    'screen' {
      $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
      $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
      $g = [System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen(0, 0, 0, 0, $bmp.Size)
      $bmp.Save((Join-Path $scratch "$arg.png")); $g.Dispose(); $bmp.Dispose()
    }
  }
}

