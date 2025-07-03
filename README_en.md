# Xiaomi PC Manager Lite

## Introduction
Xiaomi PC Manager Lite is a lightweight Windows tool designed for Xiaomi laptops, supporting battery care charging, performance mode switching, global hotkeys, tray operations, and auto-start features.

![Main GUI](gui_shot.png)

## Main Features
- Battery care charging switch and charge limit setting (supports hotkeys)
- Performance mode switching (supports hotkeys and custom mode switching)
- Tray icon and menu, supports minimizing to tray
- Auto-start option
- Window semi-transparent beautification, draggable, transparent controls

## Hotkeys
- `Ctrl+Alt+B`: Enable/disable battery care charging
- `Ctrl+Alt+P`: Switch to custom performance mode, press again to switch back to the original performance mode

## Build & Run
1. Dependencies:
   - Windows 10/11
   - Visual Studio 2017 or above
   - WinRing0 driver (WinRing0x64.dll/WinRing0.dll, must be placed in the program directory)
2. Compile:
   - Open the solution and build the `xiaomi_pc_manager_lite` project
3. Run:
   - Run as administrator (required by WinRing0)

## Notes
- Only supports some Xiaomi laptop models (must support related EC registers).
- Must be run as administrator, otherwise hardware interfaces cannot be accessed.
- WinRing0 driver copyright belongs to OpenLibSys.org.

## Acknowledgements
- [WinRing0](http://openlibsys.org/) (hardware access driver, BSD License)

## Author
- CHHHHHHEN

## License
- For study and personal use only, commercial use is prohibited.
