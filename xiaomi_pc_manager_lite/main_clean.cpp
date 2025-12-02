#include <windows.h>
#include <commctrl.h>
#include <shellapi.h>
#include <shlwapi.h>
#include <strsafe.h>
#include <string>
#include <memory>
#include <fstream>
#include "resource.h"

#pragma comment(lib, "comctl32.lib")
#pragma comment(lib, "shell32.lib")
#pragma comment(lib, "shlwapi.lib")

// 窗口类名和标题
const wchar_t* CLASS_NAME = L"XiaomiPCManagerLite";
const wchar_t* WINDOW_TITLE = L"小米电脑管家精简版";

// 控件ID
#define ID_BATTERY_CARE_ENABLE      1001
#define ID_BATTERY_CARE_LEVEL       1002
#define ID_PERFORMANCE_MODE         1003
#define ID_MIN_BUTTON               1004
#define ID_CLOSE_BUTTON             1005
#define ID_BATTERY_LEVEL_EDIT       1010
#define ID_BATTERY_LEVEL_SPIN       1011
#define ID_REFRESH_BUTTON           1013
#define ID_APPLY_BUTTON             1018
#define ID_EC_STATUS_BATTERY        1019
#define ID_EC_STATUS_PERF           1020

// 托盘相关
#define WM_TRAYICON   (WM_USER + 1)
#define ID_TRAY_EXIT        2001
#define ID_TRAY_SHOW   2002
#define ID_TRAY_BATTERY_ENABLE     2003
#define ID_TRAY_BATTERY_DISABLE    2004
#define ID_TRAY_PERF_ECO           2005
#define ID_TRAY_PERF_QUIET     2006
#define ID_TRAY_PERF_SMART         2007
#define ID_TRAY_PERF_FAST          2008
#define ID_TRAY_PERF_EXTREME   2009

// EC地址定义
#define EC_BATTERY_CARE_ADDR     0xA4
#define EC_BATTERY_LEVEL_ADDR      0xA7
#define EC_PERFORMANCE_MODE_ADDR   0x68

// 性能模式值
#define PERF_ECO_MODE       0x0A
#define PERF_QUIET_MODE        0x02
#define PERF_SMART_MODE        0x09
#define PERF_FAST_MODE  0x03
#define PERF_EXTREME_MODE          0x04

// 配置文件路径
const wchar_t* CONFIG_FILE = L"xiaomi_pc_manager_lite_config.ini";

// WinRing0函数指针类型定义
typedef BOOL(WINAPI* InitializeOls_t)();
typedef VOID(WINAPI* DeinitializeOls_t)();
typedef BYTE(WINAPI* ReadIoPortByte_t)(WORD port);
typedef VOID(WINAPI* WriteIoPortByte_t)(WORD port, BYTE value);

// 全局变量
HWND g_hWnd = nullptr;
HWND g_hBatteryCareCheck = nullptr;
HWND g_hBatteryLevelEdit = nullptr;
HWND g_hBatteryLevelSpin = nullptr;
HWND g_hBatteryLevelLabel = nullptr;
HWND g_hPerformanceCombo = nullptr;
HWND g_hTitleLabel = nullptr;
HWND g_hPerfLabel = nullptr;
HWND g_hApplyButton = nullptr;
HWND g_hEcStatusBattery = nullptr;
HWND g_hEcStatusPerf = nullptr;
NOTIFYICONDATA g_nid = {};
bool g_isMinimized = false;
bool g_isDragging = false;
POINT g_dragOffset = {};
HINSTANCE g_hInstance = nullptr;
HBRUSH g_hBackgroundBrush = nullptr;

// WinRing0相关
HMODULE g_hWinRing0 = nullptr;
bool g_winRing0Initialized = false;
InitializeOls_t InitializeOls = nullptr;
DeinitializeOls_t DeinitializeOls = nullptr;
ReadIoPortByte_t ReadIoPortByte = nullptr;
WriteIoPortByte_t WriteIoPortByte = nullptr;

// 函数声明
LRESULT CALLBACK WindowProc(HWND hwnd, UINT uMsg, WPARAM wParam, LPARAM lParam);
bool LoadWinRing0();
void UnloadWinRing0();
bool InitializeWinRing0();
void DeinitializeWinRing0();
BYTE ReadEC(WORD address);
void WriteEC(WORD address, BYTE value);
void UpdateBatteryCareStatus();
void UpdatePerformanceMode();
void SetBatteryCare(bool enable, int level = 80);
void SetPerformanceMode(int mode);
void CreateTrayIcon();
void RemoveTrayIcon();
void ShowTrayMenu();
void ToggleMainWindow();
void CreateControls(HWND hwnd);
void DrawBackground(HDC hdc, RECT* rect);
void SaveConfig();
void LoadConfig();

// 加载WinRing0 DLL
bool LoadWinRing0() {
	// 尝试加载64位版本
#ifdef _WIN64
	g_hWinRing0 = LoadLibrary(L"WinRing0x64.dll");
#else
	g_hWinRing0 = LoadLibrary(L"WinRing0.dll");
#endif

	if (!g_hWinRing0) {
		MessageBox(nullptr, L"无法加载WinRing0库，请确保DLL文件在程序目录中。", L"错误", MB_OK | MB_ICONERROR);
		return false;
	}

	// 获取函数地址
	InitializeOls = (InitializeOls_t)GetProcAddress(g_hWinRing0, "InitializeOls");
	DeinitializeOls = (DeinitializeOls_t)GetProcAddress(g_hWinRing0, "DeinitializeOls");
	ReadIoPortByte = (ReadIoPortByte_t)GetProcAddress(g_hWinRing0, "ReadIoPortByte");
	WriteIoPortByte = (WriteIoPortByte_t)GetProcAddress(g_hWinRing0, "WriteIoPortByte");

	if (!InitializeOls || !DeinitializeOls || !ReadIoPortByte || !WriteIoPortByte) {
		MessageBox(nullptr, L"WinRing0库函数加载失败。", L"错误", MB_OK | MB_ICONERROR);
		UnloadWinRing0();
		return false;
	}

	return true;
}

// 卸载WinRing0 DLL
void UnloadWinRing0() {
	if (g_hWinRing0) {
		FreeLibrary(g_hWinRing0);
		g_hWinRing0 = nullptr;
	}
	InitializeOls = nullptr;
	DeinitializeOls = nullptr;
	ReadIoPortByte = nullptr;
	WriteIoPortByte = nullptr;
}

// 初始化WinRing0
bool InitializeWinRing0() {
	if (!LoadWinRing0()) {
		return false;
	}

	if (!InitializeOls()) {
		MessageBox(nullptr, L"无法初始化WinRing0库，请确保以管理员权限运行程序。", L"错误", MB_OK | MB_ICONERROR);
		UnloadWinRing0();
		return false;
	}
	g_winRing0Initialized = true;
	return true;
}

// 反初始化WinRing0
void DeinitializeWinRing0() {
	if (g_winRing0Initialized && DeinitializeOls) {
		DeinitializeOls();
		g_winRing0Initialized = false;
	}
	UnloadWinRing0();
}

// EC端口定义
#define EC_DATA_PORT    0x62
#define EC_CMD_PORT     0x66

// 等待EC准备就绪
bool WaitECReady() {
	int timeout = 1000;
	while (timeout-- > 0) {
		if (ReadIoPortByte && (ReadIoPortByte(EC_CMD_PORT) & 0x02) == 0) {
			return true;
		}
		Sleep(1);
	}
	return false;
}

// 读取EC数据
BYTE ReadEC(WORD address) {
	if (!g_winRing0Initialized || !ReadIoPortByte || !WriteIoPortByte) return 0;

	if (!WaitECReady()) return 0;

	// 发送读命令
	WriteIoPortByte(EC_CMD_PORT, 0x80);

	if (!WaitECReady()) return 0;

	// 发送地址
	WriteIoPortByte(EC_DATA_PORT, (BYTE)address);

	if (!WaitECReady()) return 0;

	// 读取数据
	return ReadIoPortByte(EC_DATA_PORT);
}

// 写入EC数据
void WriteEC(WORD address, BYTE value) {
	if (!g_winRing0Initialized || !ReadIoPortByte || !WriteIoPortByte) return;

	if (!WaitECReady()) return;

	// 发送写命令
	WriteIoPortByte(EC_CMD_PORT, 0x81);

	if (!WaitECReady()) return;

	// 发送地址
	WriteIoPortByte(EC_DATA_PORT, (BYTE)address);

	if (!WaitECReady()) return;

	// 发送数据
	WriteIoPortByte(EC_DATA_PORT, value);
}

// 更新养护充电状态显示（仅显示，不应用）
void UpdateBatteryCareStatus() {
	BYTE ecValue = ReadEC(EC_BATTERY_CARE_ADDR);
	bool isEnabled = (ecValue & 0x01) != 0;
	BYTE level = ReadEC(EC_BATTERY_LEVEL_ADDR);
	if (level > 100) level = 80;

	// 在EC状态显示区域显示当前EC中的值
	wchar_t statusText[128];
	if (isEnabled) {
		swprintf_s(statusText, L"电池养护: 已启用 (上限 %d%%)", level);
	}
	else {
		swprintf_s(statusText, L"电池养护: 已禁用");
	}
	SetWindowText(g_hEcStatusBattery, statusText);
	// 强制重绘控件以避免文字重叠
	InvalidateRect(g_hEcStatusBattery, nullptr, TRUE);
	UpdateWindow(g_hEcStatusBattery);
}

// 更新性能模式显示（仅显示，不应用）
void UpdatePerformanceMode() {
	BYTE mode = ReadEC(EC_PERFORMANCE_MODE_ADDR);
	
	const wchar_t* modeName = L"未知";
	switch (mode) {
	case PERF_ECO_MODE: modeName = L"省电模式"; break;
	case PERF_QUIET_MODE: modeName = L"静谧模式"; break;
	case PERF_SMART_MODE: modeName = L"智能模式"; break;
	case PERF_FAST_MODE: modeName = L"极速模式"; break;
	case PERF_EXTREME_MODE: modeName = L"狂暴模式"; break;
	}

	wchar_t statusText[128];
	swprintf_s(statusText, L"性能模式: %s", modeName);
	SetWindowText(g_hEcStatusPerf, statusText);
	// 强制重绘控件以避免文字重叠
	InvalidateRect(g_hEcStatusPerf, nullptr, TRUE);
	UpdateWindow(g_hEcStatusPerf);
}

// 设置养护充电
void SetBatteryCare(bool enable, int level) {
	if (level < 0) level = 0;
	if (level > 100) level = 100;

	// 先写入电池等级，再设置养护模式
	if (enable) {
		WriteEC(EC_BATTERY_LEVEL_ADDR, (BYTE)level);
	}

	BYTE currentValue = ReadEC(EC_BATTERY_CARE_ADDR);
	if (enable) {
		currentValue |= 0x01;
	}
	else {
		currentValue &= 0xFE;
	}
	WriteEC(EC_BATTERY_CARE_ADDR, currentValue);
}

// 设置性能模式
void SetPerformanceMode(int mode) {
	BYTE modeValue = PERF_SMART_MODE;
	switch (mode) {
	case 0: modeValue = PERF_ECO_MODE; break;
	case 1: modeValue = PERF_QUIET_MODE; break;
	case 2: modeValue = PERF_SMART_MODE; break;
	case 3: modeValue = PERF_FAST_MODE; break;
	case 4: modeValue = PERF_EXTREME_MODE; break;
	}
	WriteEC(EC_PERFORMANCE_MODE_ADDR, modeValue);
}

// 保存配置到文件
void SaveConfig() {
	if (!g_hBatteryCareCheck || !g_hBatteryLevelEdit || !g_hPerformanceCombo)
		return;

	std::wofstream ofs(CONFIG_FILE);
	if (!ofs.is_open() || !ofs.good()) return;

	int batteryCare = SendMessage(g_hBatteryCareCheck, BM_GETCHECK, 0, 0) == BST_CHECKED ? 1 : 0;

	wchar_t levelText[16] = { 0 };
	if (GetWindowText(g_hBatteryLevelEdit, levelText, 16) == 0) wcscpy_s(levelText, L"80");
	int batteryLevel = _wtoi(levelText);

	int perfMode = (int)SendMessage(g_hPerformanceCombo, CB_GETCURSEL, 0, 0);

	ofs << L"battery_care=" << batteryCare << std::endl;
	ofs << L"battery_level=" << batteryLevel << std::endl;
	ofs << L"perf_mode=" << perfMode << std::endl;
	ofs.close();
}

// 从文件读取配置
void LoadConfig() {
	std::wifstream ifs(CONFIG_FILE);
	if (!ifs) return;

	std::wstring line;
	int batteryCare = -1, batteryLevel = -1, perfMode = -1;

	while (std::getline(ifs, line)) {
		if (line.find(L"battery_care=") == 0) batteryCare = std::stoi(line.substr(13));
		else if (line.find(L"battery_level=") == 0) batteryLevel = std::stoi(line.substr(14));
		else if (line.find(L"perf_mode=") == 0) perfMode = std::stoi(line.substr(10));
	}

	if (batteryCare != -1)
		SendMessage(g_hBatteryCareCheck, BM_SETCHECK, batteryCare ? BST_CHECKED : BST_UNCHECKED, 0);

	if (batteryLevel != -1) {
		wchar_t buf[16];
		swprintf_s(buf, L"%d", batteryLevel);
		SetWindowText(g_hBatteryLevelEdit, buf);
		SendMessage(g_hBatteryLevelSpin, UDM_SETPOS32, 0, batteryLevel);
	}

	if (perfMode != -1)
		SendMessage(g_hPerformanceCombo, CB_SETCURSEL, perfMode, 0);
}

// 创建托盘图标
void CreateTrayIcon() {
	ZeroMemory(&g_nid, sizeof(NOTIFYICONDATA));
	g_nid.cbSize = sizeof(NOTIFYICONDATA);
	g_nid.hWnd = g_hWnd;
	g_nid.uID = 1;
	g_nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
	g_nid.uCallbackMessage = WM_TRAYICON;

	g_nid.hIcon = LoadIcon(g_hInstance, MAKEINTRESOURCE(IDI_TRAY_ICON));
	if (!g_nid.hIcon) {
		g_nid.hIcon = LoadIcon(nullptr, IDI_APPLICATION);
	}

	wcscpy_s(g_nid.szTip, sizeof(g_nid.szTip) / sizeof(wchar_t), L"小米电脑管家精简版");

	if (!Shell_NotifyIcon(NIM_ADD, &g_nid)) {
		MessageBox(g_hWnd, L"创建托盘图标失败。", L"警告", MB_OK | MB_ICONWARNING);
	}
}

// 移除托盘图标
void RemoveTrayIcon() {
	Shell_NotifyIcon(NIM_DELETE, &g_nid);
}

// 显示托盘菜单
void ShowTrayMenu() {
	POINT pt;
	GetCursorPos(&pt);

	HMENU hMenu = CreatePopupMenu();
	AppendMenu(hMenu, MF_STRING, ID_TRAY_SHOW, L"显示主界面");
	AppendMenu(hMenu, MF_SEPARATOR, 0, nullptr);

	HMENU hBatteryMenu = CreatePopupMenu();
	AppendMenu(hBatteryMenu, MF_STRING, ID_TRAY_BATTERY_ENABLE, L"开启养护充电");
	AppendMenu(hBatteryMenu, MF_STRING, ID_TRAY_BATTERY_DISABLE, L"关闭养护充电");
	AppendMenu(hMenu, MF_POPUP, (UINT_PTR)hBatteryMenu, L"养护充电");

	HMENU hPerfMenu = CreatePopupMenu();
	AppendMenu(hPerfMenu, MF_STRING, ID_TRAY_PERF_ECO, L"省电模式");
	AppendMenu(hPerfMenu, MF_STRING, ID_TRAY_PERF_QUIET, L"静谧模式");
	AppendMenu(hPerfMenu, MF_STRING, ID_TRAY_PERF_SMART, L"智能模式");
	AppendMenu(hPerfMenu, MF_STRING, ID_TRAY_PERF_FAST, L"极速模式");
	AppendMenu(hPerfMenu, MF_STRING, ID_TRAY_PERF_EXTREME, L"狂暴模式");
	AppendMenu(hMenu, MF_POPUP, (UINT_PTR)hPerfMenu, L"性能模式");

	AppendMenu(hMenu, MF_SEPARATOR, 0, nullptr);
	AppendMenu(hMenu, MF_STRING, ID_TRAY_EXIT, L"退出");

	SetForegroundWindow(g_hWnd);
	TrackPopupMenu(hMenu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, g_hWnd, nullptr);
	DestroyMenu(hMenu);
}

// 切换主窗口显示状态
void ToggleMainWindow() {
	if (g_isMinimized || !IsWindowVisible(g_hWnd)) {
		ShowWindow(g_hWnd, SW_SHOW);
		ShowWindow(g_hWnd, SW_RESTORE);
		SetForegroundWindow(g_hWnd);
		BringWindowToTop(g_hWnd);
		g_isMinimized = false;
	}
	else {
		ShowWindow(g_hWnd, SW_HIDE);
		g_isMinimized = true;
	}
}

// 创建控件
void CreateControls(HWND hwnd) {
	int left = 24;
	int width = 320;
	int y = 24;
	int spacing = 12;

	g_hTitleLabel = CreateWindow(L"STATIC", L"小米电脑管家精简版",
		WS_VISIBLE | WS_CHILD | SS_LEFT,
		left, y, width, 36, hwnd, nullptr, GetModuleHandle(nullptr), nullptr);
	y += 36 + spacing;

	g_hBatteryCareCheck = CreateWindow(L"BUTTON", L"开启养护充电",
		WS_VISIBLE | WS_CHILD | BS_AUTOCHECKBOX,
		left, y, width, 28, hwnd, (HMENU)ID_BATTERY_CARE_ENABLE, GetModuleHandle(nullptr), nullptr);
	y += 28 + spacing;

	g_hBatteryLevelLabel = CreateWindow(L"STATIC", L"充电上限 (0-100%):",
		WS_VISIBLE | WS_CHILD | SS_LEFT,
		left, y, width, 22, hwnd, nullptr, GetModuleHandle(nullptr), nullptr);
	y += 22 + spacing;

	g_hBatteryLevelEdit = CreateWindow(L"EDIT", L"80",
		WS_VISIBLE | WS_CHILD | WS_BORDER | ES_NUMBER | ES_RIGHT,
		left, y, 60, 28, hwnd, (HMENU)ID_BATTERY_LEVEL_EDIT, GetModuleHandle(nullptr), nullptr);
	g_hBatteryLevelSpin = CreateWindow(UPDOWN_CLASS, L"",
		WS_VISIBLE | WS_CHILD | UDS_SETBUDDYINT | UDS_ALIGNRIGHT | UDS_ARROWKEYS | UDS_NOTHOUSANDS,
		left + 60, y, 20, 28, hwnd, (HMENU)ID_BATTERY_LEVEL_SPIN, GetModuleHandle(nullptr), nullptr);
	SendMessage(g_hBatteryLevelSpin, UDM_SETBUDDY, (WPARAM)g_hBatteryLevelEdit, 0);
	SendMessage(g_hBatteryLevelSpin, UDM_SETRANGE32, 0, 100);
	SendMessage(g_hBatteryLevelSpin, UDM_SETPOS32, 0, 80);
	CreateWindow(L"STATIC", L"%",
		WS_VISIBLE | WS_CHILD,
		left + 85, y + 4, 20, 20, hwnd, nullptr, GetModuleHandle(nullptr), nullptr);
	y += 28 + spacing;

	g_hPerfLabel = CreateWindow(L"STATIC", L"性能模式:",
		WS_VISIBLE | WS_CHILD | SS_LEFT,
		left, y, width, 22, hwnd, nullptr, GetModuleHandle(nullptr), nullptr);
	y += 22 + spacing;

	g_hPerformanceCombo = CreateWindow(WC_COMBOBOX, L"",
		WS_VISIBLE | WS_CHILD | CBS_DROPDOWNLIST | CBS_HASSTRINGS,
		left, y, width, 120, hwnd, (HMENU)ID_PERFORMANCE_MODE, GetModuleHandle(nullptr), nullptr);
	SendMessage(g_hPerformanceCombo, CB_ADDSTRING, 0, (LPARAM)L"省电模式");
	SendMessage(g_hPerformanceCombo, CB_ADDSTRING, 0, (LPARAM)L"静谧模式");
	SendMessage(g_hPerformanceCombo, CB_ADDSTRING, 0, (LPARAM)L"智能模式");
	SendMessage(g_hPerformanceCombo, CB_ADDSTRING, 0, (LPARAM)L"极速模式");
	SendMessage(g_hPerformanceCombo, CB_ADDSTRING, 0, (LPARAM)L"狂暴模式");
	SendMessage(g_hPerformanceCombo, CB_SETCURSEL, 2, 0);
	y += 32 + spacing;

	CreateWindow(L"BUTTON", L"刷新状态",
		WS_VISIBLE | WS_CHILD | BS_PUSHBUTTON,
		left, y, 110, 32, hwnd, (HMENU)ID_REFRESH_BUTTON, GetModuleHandle(nullptr), nullptr);
	g_hApplyButton = CreateWindow(L"BUTTON", L"应用设置",
		WS_VISIBLE | WS_CHILD | BS_PUSHBUTTON,
		left + 120, y, 110, 32, hwnd, (HMENU)ID_APPLY_BUTTON, GetModuleHandle(nullptr), nullptr);
	y += 32 + spacing + 8;

	// EC状态显示区域 - 使用更大的宽度避免文字重叠
	CreateWindow(L"STATIC", L"════ 当前EC状态 ════",
		WS_VISIBLE | WS_CHILD | SS_CENTER,
		left, y, width + 280, 20, hwnd, nullptr, GetModuleHandle(nullptr), nullptr);
	y += 20 + 8;

	g_hEcStatusBattery = CreateWindow(L"STATIC", L"电池养护: 未读取",
		WS_VISIBLE | WS_CHILD | SS_LEFT,
		left, y, width + 280, 22, hwnd, (HMENU)ID_EC_STATUS_BATTERY, GetModuleHandle(nullptr), nullptr);
	y += 22 + 6;

	g_hEcStatusPerf = CreateWindow(L"STATIC", L"性能模式: 未读取",
		WS_VISIBLE | WS_CHILD | SS_LEFT,
		left, y, width + 280, 22, hwnd, (HMENU)ID_EC_STATUS_PERF, GetModuleHandle(nullptr), nullptr);
	y += 22 + spacing;

	// 窗口控制按钮 - 使用GetClientRect获取实际窗口宽度
	RECT clientRect;
	GetClientRect(hwnd, &clientRect);
	int windowWidth = clientRect.right;

	CreateWindow(L"BUTTON", L"—",
		WS_VISIBLE | WS_CHILD | BS_PUSHBUTTON | BS_FLAT,
		windowWidth - 80, 8, 32, 28, hwnd, (HMENU)ID_MIN_BUTTON, GetModuleHandle(nullptr), nullptr);
	CreateWindow(L"BUTTON", L"×",
		WS_VISIBLE | WS_CHILD | BS_PUSHBUTTON | BS_FLAT,
		windowWidth - 40, 8, 32, 28, hwnd, (HMENU)ID_CLOSE_BUTTON, GetModuleHandle(nullptr), nullptr);
}

// 绘制背景
void DrawBackground(HDC hdc, RECT* rect) {
	// 使用纯色背景
	HBRUSH hBrush = CreateSolidBrush(RGB(245, 250, 255));
	FillRect(hdc, rect, hBrush);
	DeleteObject(hBrush);

	// 绘制边框
	HPEN hPen = CreatePen(PS_SOLID, 2, RGB(100, 149, 237));
	HPEN hOldPen = (HPEN)SelectObject(hdc, hPen);
	MoveToEx(hdc, 0, 0, nullptr);
	LineTo(hdc, rect->right - 1, 0);
	LineTo(hdc, rect->right - 1, rect->bottom - 1);
	LineTo(hdc, 0, rect->bottom - 1);
	LineTo(hdc, 0, 0);
	SelectObject(hdc, hOldPen);
	DeleteObject(hPen);
}

// 窗口过程
LRESULT CALLBACK WindowProc(HWND hwnd, UINT uMsg, WPARAM wParam, LPARAM lParam) {
	switch (uMsg) {
	case WM_CREATE:
		CreateControls(hwnd);
		if (InitializeWinRing0()) {
			LoadConfig();
		}
		else {
			LoadConfig();
		}
		// 不再自动从EC读取状态
		// 用户需要点击"刷新状态"按钮手动读取
		SetTimer(hwnd, 1, 1000, nullptr);
		break;

	case WM_POWERBROADCAST:
		if (wParam == PBT_APMPOWERSTATUSCHANGE) {
			SetTimer(hwnd, 100, 3000, NULL);
		}
		break;

	case WM_TIMER:
		if (wParam == 1) {
			KillTimer(hwnd, 1);
			CreateTrayIcon();
		}
		else if (wParam == 100) {
			KillTimer(hwnd, 100);
			// 电源状态改变时自动重新应用设置
			bool careChecked = SendMessage(g_hBatteryCareCheck, BM_GETCHECK, 0, 0) == BST_CHECKED;
			wchar_t levelText[16];
			GetWindowText(g_hBatteryLevelEdit, levelText, 16);
			int careLevel = _wtoi(levelText);
			if (careLevel < 0) careLevel = 0;
			if (careLevel > 100) careLevel = 100;
			SetBatteryCare(careChecked, careLevel);
			
			int sel = (int)SendMessage(g_hPerformanceCombo, CB_GETCURSEL, 0, 0);
			SetPerformanceMode(sel);
		}
		break;

	case WM_PAINT: {
		PAINTSTRUCT ps;
		HDC hdc = BeginPaint(hwnd, &ps);
		RECT rect;
		GetClientRect(hwnd, &rect);
		DrawBackground(hdc, &rect);
		EndPaint(hwnd, &ps);
		break;
	}

	case WM_ERASEBKGND:
		return 1;

	case WM_LBUTTONDOWN:
		g_isDragging = true;
		g_dragOffset.x = LOWORD(lParam);
		g_dragOffset.y = HIWORD(lParam);
		SetCapture(hwnd);
		break;

	case WM_LBUTTONUP:
		if (g_isDragging) {
			g_isDragging = false;
			ReleaseCapture();
		}
		break;

	case WM_MOUSEMOVE:
		if (g_isDragging) {
			POINT pt;
			GetCursorPos(&pt);
			SetWindowPos(hwnd, nullptr, pt.x - g_dragOffset.x, pt.y - g_dragOffset.y, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
		}
		break;

	case WM_CTLCOLORSTATIC: {
		HDC hdcStatic = (HDC)wParam;
		HWND hStatic = (HWND)lParam;
		
		// 检查是否是EC状态显示控件
		if (hStatic == g_hEcStatusBattery || hStatic == g_hEcStatusPerf) {
			// EC状态标签使用不透明背景以避免文字重叠
			SetBkMode(hdcStatic, OPAQUE);
			SetBkColor(hdcStatic, RGB(245, 250, 255));
			SetTextColor(hdcStatic, RGB(51, 51, 51));
			if (!g_hBackgroundBrush) {
				g_hBackgroundBrush = CreateSolidBrush(RGB(245, 250, 255));
			}
			return (LRESULT)g_hBackgroundBrush;
		}
		
		// 其他控件使用透明背景
		SetBkMode(hdcStatic, TRANSPARENT);
		SetTextColor(hdcStatic, RGB(51, 51, 51));
		return (LRESULT)GetStockObject(NULL_BRUSH);
	}

	case WM_COMMAND:
		switch (LOWORD(wParam)) {
		case ID_BATTERY_CARE_ENABLE:
			// 只更新控件状态，不写入EC
			// 用户需要点击"应用设置"按钮才会写入EC
			break;

		case ID_BATTERY_LEVEL_EDIT:
			// 只更新编辑框内容，不写入EC
			// 用户需要点击"应用设置"按钮才会写入EC
			break;

		case ID_PERFORMANCE_MODE:
			// 只更新下拉框选择，不写入EC
			// 用户需要点击"应用设置"按钮才会写入EC
			break;

		case ID_MIN_BUTTON:
			ShowWindow(hwnd, SW_HIDE);
			g_isMinimized = true;
			if (g_nid.hWnd == nullptr) {
				CreateTrayIcon();
			}
			else {
				Shell_NotifyIcon(NIM_MODIFY, &g_nid);
			}
			break;

		case ID_CLOSE_BUTTON:
			DestroyWindow(hwnd);
			break;

		case ID_REFRESH_BUTTON:
			UpdateBatteryCareStatus();
			UpdatePerformanceMode();
			break;

		case ID_APPLY_BUTTON:
			{
				// 应用所有设置到EC
				bool isChecked = SendMessage(g_hBatteryCareCheck, BM_GETCHECK, 0, 0) == BST_CHECKED;
				wchar_t text[16];
				GetWindowText(g_hBatteryLevelEdit, text, 16);
				int level = _wtoi(text);
				if (level < 0) level = 0;
				if (level > 100) level = 100;
				SetBatteryCare(isChecked, level);

				int sel = (int)SendMessage(g_hPerformanceCombo, CB_GETCURSEL, 0, 0);
				SetPerformanceMode(sel);

				MessageBox(hwnd, L"设置已应用到EC", L"提示", MB_OK | MB_ICONINFORMATION);
				break;
			}

		case ID_TRAY_SHOW:
			ToggleMainWindow();
			break;

		case ID_TRAY_EXIT:
			DestroyWindow(hwnd);
			break;

		case ID_TRAY_BATTERY_ENABLE:
			SetBatteryCare(true, 80);
			break;

		case ID_TRAY_BATTERY_DISABLE:
			SetBatteryCare(false);
			break;

		case ID_TRAY_PERF_ECO:
			SetPerformanceMode(0);
			break;

		case ID_TRAY_PERF_QUIET:
			SetPerformanceMode(1);
			break;

		case ID_TRAY_PERF_SMART:
			SetPerformanceMode(2);
			break;

		case ID_TRAY_PERF_FAST:
			SetPerformanceMode(3);
			break;

		case ID_TRAY_PERF_EXTREME:
			SetPerformanceMode(4);
			break;
		}
		break;

	case WM_TRAYICON:
		switch (lParam) {
		case WM_LBUTTONDBLCLK:
		case WM_LBUTTONUP:
			ToggleMainWindow();
			break;
		case WM_RBUTTONUP:
		case WM_CONTEXTMENU:
			ShowTrayMenu();
			break;
		}
		break;

	case WM_ENDSESSION:
		if (wParam == TRUE) {
			SaveConfig();
		}
		break;

	case WM_DESTROY:
		SaveConfig();
		RemoveTrayIcon();
		DeinitializeWinRing0();
		if (g_hBackgroundBrush) {
			DeleteObject(g_hBackgroundBrush);
			g_hBackgroundBrush = nullptr;
		}
		PostQuitMessage(0);
		break;

	case WM_SYSCOMMAND:
		if (wParam == SC_MINIMIZE) {
			ShowWindow(hwnd, SW_HIDE);
			g_isMinimized = true;
			return 0;
		}
		return DefWindowProc(hwnd, uMsg, wParam, lParam);

	default:
		return DefWindowProc(hwnd, uMsg, wParam, lParam);
	}
	return 0;
}

// 主函数
int WINAPI wWinMain(HINSTANCE hInstance, HINSTANCE hPrevInstance, LPWSTR lpCmdLine, int nCmdShow) {
	g_hInstance = hInstance;

	CoInitializeEx(NULL, COINIT_MULTITHREADED);

	wchar_t exePath[MAX_PATH];
	GetModuleFileNameW(nullptr, exePath, MAX_PATH);
	wchar_t* lastSlash = wcsrchr(exePath, L'\\');
	if (lastSlash) {
		*lastSlash = L'\0';
		SetCurrentDirectoryW(exePath);
	}

	INITCOMMONCONTROLSEX icex;
	icex.dwSize = sizeof(INITCOMMONCONTROLSEX);
	icex.dwICC = ICC_STANDARD_CLASSES | ICC_BAR_CLASSES;
	InitCommonControlsEx(&icex);

	WNDCLASS wc = {};
	wc.lpfnWndProc = WindowProc;
	wc.hInstance = hInstance;
	wc.lpszClassName = CLASS_NAME;
	wc.hbrBackground = nullptr;
	wc.hCursor = LoadCursor(nullptr, IDC_ARROW);
	wc.hIcon = LoadIcon(hInstance, MAKEINTRESOURCE(IDI_MAIN_ICON));
	if (!wc.hIcon) {
		wc.hIcon = LoadIcon(nullptr, IDI_APPLICATION);
	}

	if (!RegisterClass(&wc)) {
		MessageBox(nullptr, L"注册窗口类失败", L"错误", MB_OK | MB_ICONERROR);
		return 0;
	}

	const int windowWidth = 650;
	const int windowHeight = 420;  // 减小高度，移除了键盘背光控件
	g_hWnd = CreateWindowEx(
		0,
		CLASS_NAME,
		WINDOW_TITLE,
		WS_POPUP | WS_VISIBLE,
		(GetSystemMetrics(SM_CXSCREEN) - windowWidth) / 2,
		(GetSystemMetrics(SM_CYSCREEN) - windowHeight) / 2,
		windowWidth, windowHeight,
		nullptr, nullptr, hInstance, nullptr
	);

	if (g_hWnd == nullptr) {
		MessageBox(nullptr, L"创建窗口失败", L"错误", MB_OK | MB_ICONERROR);
		return 0;
	}

	SetWindowLongPtr(g_hWnd, GWL_EXSTYLE, GetWindowLongPtr(g_hWnd, GWL_EXSTYLE) | WS_EX_LAYERED);
	SetLayeredWindowAttributes(g_hWnd, 0, 180, LWA_ALPHA);

	ShowWindow(g_hWnd, nCmdShow);
	UpdateWindow(g_hWnd);
	InvalidateRect(g_hWnd, nullptr, TRUE);

	MSG msg = {};
	while (GetMessage(&msg, nullptr, 0, 0)) {
		TranslateMessage(&msg);
		DispatchMessage(&msg);
	}

	CoUninitialize();

	return (int)msg.wParam;
}
