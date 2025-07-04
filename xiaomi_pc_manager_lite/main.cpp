#include <windows.h>
#include <commctrl.h>
#include <shellapi.h>
#include <gdiplus.h>
#include <shlwapi.h>
#include <strsafe.h>
#include <string>
#include <memory>
#include <fstream>
#include <taskschd.h>
#include <comdef.h>
#include "resource.h"

#pragma comment(lib, "comctl32.lib")
#pragma comment(lib, "shell32.lib")
#pragma comment(lib, "gdiplus.lib")
#pragma comment(lib, "shlwapi.lib")
#pragma comment(lib, "taskschd.lib")
#pragma comment(lib, "comsupp.lib")

// 窗口类名和标题
const wchar_t* CLASS_NAME = L"XiaomiPCManagerLite";
const wchar_t* WINDOW_TITLE = L"小米电脑管家精简版";

// 控件ID
#define ID_BATTERY_CARE_ENABLE      1001
#define ID_BATTERY_CARE_LEVEL       1002
#define ID_PERFORMANCE_MODE         1003
#define ID_MIN_BUTTON              1004
#define ID_CLOSE_BUTTON            1005
#define ID_BATTERY_LEVEL_EDIT       1010
#define ID_BATTERY_LEVEL_SPIN       1011
#define ID_AUTO_START_CHECK         1012
#define ID_REFRESH_BUTTON           1013

// 托盘相关
#define WM_TRAYICON                 (WM_USER + 1)
#define ID_TRAY_EXIT               2001
#define ID_TRAY_SHOW               2002
#define ID_TRAY_BATTERY_ENABLE     2003
#define ID_TRAY_BATTERY_DISABLE    2004
#define ID_TRAY_PERF_ECO           2005
#define ID_TRAY_PERF_QUIET         2006
#define ID_TRAY_PERF_SMART         2007
#define ID_TRAY_PERF_FAST          2008
#define ID_TRAY_PERF_EXTREME       2009

// EC地址定义
#define EC_BATTERY_CARE_ADDR       0xA4
#define EC_BATTERY_LEVEL_ADDR      0xA7
#define EC_PERFORMANCE_MODE_ADDR   0x68

// 性能模式值
#define PERF_ECO_MODE              0x0A
#define PERF_QUIET_MODE            0x02
#define PERF_SMART_MODE            0x09
#define PERF_FAST_MODE             0x03
#define PERF_EXTREME_MODE          0x04

// 热键ID
#define HOTKEY_ID_BATTERY  1
#define HOTKEY_ID_PERF     2

// 注册表自启动项名
const wchar_t* AUTOSTART_REG_PATH = L"Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const wchar_t* AUTOSTART_REG_NAME = L"XiaomiPCManagerLite";

// 配置文件路径
const wchar_t* CONFIG_FILE = L"xiaomi_pc_manager_lite_config.ini";
const wchar_t* TASK_NAME = L"XiaomiPCManagerLite_AutoStart";

// WinRing0函数指针类型定义
typedef BOOL (WINAPI *InitializeOls_t)();
typedef VOID (WINAPI *DeinitializeOls_t)();
typedef BYTE (WINAPI *ReadIoPortByte_t)(WORD port);
typedef VOID (WINAPI *WriteIoPortByte_t)(WORD port, BYTE value);

// 全局变量
HWND g_hWnd = nullptr;
HWND g_hBatteryCareCheck = nullptr;
HWND g_hBatteryLevelEdit = nullptr;
HWND g_hBatteryLevelSpin = nullptr;
HWND g_hBatteryLevelLabel = nullptr;
HWND g_hPerformanceCombo = nullptr;
HWND g_hTitleLabel = nullptr; 
HWND g_hPerfLabel = nullptr; 
NOTIFYICONDATA g_nid = {};
bool g_isMinimized = false;
bool g_isDragging = false;
POINT g_dragOffset = {};
Gdiplus::Image* g_backgroundImage = nullptr;
HINSTANCE g_hInstance = nullptr;
HBRUSH g_hTransparentBrush = nullptr;
bool g_hasTransparentBackground = false;
int g_perfModeIndex = 2; // 当前性能模式索引，默认智能模式
int g_lastPerfModeIndex = 2; // 上一次性能模式索引
int g_customPerfModeIndex = 4; // 用户自定义性能模式索引，默认狂暴模式

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
void CreateProgrammaticBackground();
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
void LoadBackgroundImage();
void DrawBackground(HDC hdc, RECT* rect);
void RegisterDefaultHotKeys(HWND hwnd);
void UnregisterCustomHotKeys(HWND hwnd);
bool IsAutoStartEnabled();
void SetAutoStart(bool enable);
void SaveConfig();
void LoadConfig();

// 判断是否已设置自启动
bool IsAutoStartEnabled() {
    HRESULT hr = CoInitializeEx(NULL, COINIT_MULTITHREADED);
    if (FAILED(hr)) return false;

    ITaskService* pService = NULL;
    hr = CoCreateInstance(CLSID_TaskScheduler, NULL, CLSCTX_INPROC_SERVER, IID_ITaskService, (void**)&pService);
    if (FAILED(hr)) {
        CoUninitialize();
        return false;
    }

    hr = pService->Connect(_variant_t(), _variant_t(), _variant_t(), _variant_t());
    if (FAILED(hr)) {
        pService->Release();
        CoUninitialize();
        return false;
    }

    ITaskFolder* pRootFolder = NULL;
    hr = pService->GetFolder(_bstr_t(L""), &pRootFolder);
    if (FAILED(hr)) {
        pService->Release();
        CoUninitialize();
        return false;
    }

    IRegisteredTask* pRegisteredTask = NULL;
    hr = pRootFolder->GetTask(_bstr_t(TASK_NAME), &pRegisteredTask);
    
    pRootFolder->Release();
    pService->Release();
    CoUninitialize();

    if (SUCCEEDED(hr) && pRegisteredTask) {
        pRegisteredTask->Release();
        return true;
    }

    return false;
}

// 设置/取消自启动（使用Task Scheduler COM API）
void SetAutoStart(bool enable) {
    HRESULT hr = CoInitializeEx(NULL, COINIT_MULTITHREADED);
    if (FAILED(hr)) return;

    ITaskService* pService = NULL;
    hr = CoCreateInstance(CLSID_TaskScheduler, NULL, CLSCTX_INPROC_SERVER, IID_ITaskService, (void**)&pService);
    if (FAILED(hr)) {
        CoUninitialize();
        return;
    }

    hr = pService->Connect(_variant_t(), _variant_t(), _variant_t(), _variant_t());
    if (FAILED(hr)) {
        pService->Release();
        CoUninitialize();
        return;
    }

    ITaskFolder* pRootFolder = NULL;
    hr = pService->GetFolder(_bstr_t(L""), &pRootFolder);
    if (FAILED(hr)) {
        pService->Release();
        CoUninitialize();
        return;
    }

    if (enable) {
        // 如果任务已存在，先删除
        pRootFolder->DeleteTask(_bstr_t(TASK_NAME), 0);

        ITaskDefinition* pTask = NULL;
        hr = pService->NewTask(0, &pTask);
        if (FAILED(hr)) {
            pRootFolder->Release();
            pService->Release();
            CoUninitialize();
            return;
        }

        // 设置主体
        IPrincipal* pPrincipal = NULL;
        hr = pTask->get_Principal(&pPrincipal);
        if (SUCCEEDED(hr)) {
            pPrincipal->put_RunLevel(TASK_RUNLEVEL_HIGHEST);
            pPrincipal->Release();
        }

        // 设置触发器
        ITriggerCollection* pTriggerCollection = NULL;
        hr = pTask->get_Triggers(&pTriggerCollection);
        if (SUCCEEDED(hr)) {
            ITrigger* pTrigger = NULL;
            hr = pTriggerCollection->Create(TASK_TRIGGER_LOGON, &pTrigger);
            if (SUCCEEDED(hr)) {
                pTrigger->put_Id(_bstr_t(L"Trigger1"));
                pTrigger->Release();
            }
            pTriggerCollection->Release();
        }

        // 设置动作
        IActionCollection* pActionCollection = NULL;
        hr = pTask->get_Actions(&pActionCollection);
        if (SUCCEEDED(hr)) {
            IAction* pAction = NULL;
            hr = pActionCollection->Create(TASK_ACTION_EXEC, &pAction);
            if (SUCCEEDED(hr)) {
                IExecAction* pExecAction = NULL;
                hr = pAction->QueryInterface(IID_IExecAction, (void**)&pExecAction);
                if (SUCCEEDED(hr)) {
                    wchar_t exePath[MAX_PATH];
                    GetModuleFileNameW(NULL, exePath, MAX_PATH);
                    pExecAction->put_Path(_bstr_t(exePath));
                    pExecAction->Release();
                }
                pAction->Release();
            }
            pActionCollection->Release();
        }

        // 设置
        ITaskSettings* pSettings = NULL;
        hr = pTask->get_Settings(&pSettings);
        if (SUCCEEDED(hr)) {
            pSettings->put_DisallowStartIfOnBatteries(VARIANT_FALSE);
            pSettings->put_StopIfGoingOnBatteries(VARIANT_FALSE);
            pSettings->put_ExecutionTimeLimit(_bstr_t(L"PT0S")); // 无时间限制
            pSettings->put_StartWhenAvailable(VARIANT_TRUE);
            pSettings->Release();
        }

        // 注册任务
        IRegisteredTask* pRegisteredTask = NULL;
        pRootFolder->RegisterTaskDefinition(
            _bstr_t(TASK_NAME),
            pTask,
            TASK_CREATE_OR_UPDATE,
            _variant_t(L""), // User
            _variant_t(L""), // Password
            TASK_LOGON_INTERACTIVE_TOKEN,
            _variant_t(L""),
            &pRegisteredTask);

        if (pRegisteredTask) pRegisteredTask->Release();
        pTask->Release();
    } else {
        pRootFolder->DeleteTask(_bstr_t(TASK_NAME), 0);
    }

    pRootFolder->Release();
    pService->Release();
    CoUninitialize();
}

// 从资源加载图片
Gdiplus::Image* LoadImageFromResource(HINSTANCE hInstance, int resourceId, const wchar_t* resourceType) {
    HRSRC hResource = FindResource(hInstance, MAKEINTRESOURCE(resourceId), resourceType);
    if (!hResource) return nullptr;
    
    DWORD imageSize = SizeofResource(hInstance, hResource);
    if (imageSize == 0) return nullptr;
    
    HGLOBAL hGlobal = LoadResource(hInstance, hResource);
    if (!hGlobal) return nullptr;
    
    void* pResourceData = LockResource(hGlobal);
    if (!pResourceData) return nullptr;
    
    // 创建内存流
    HGLOBAL hMem = GlobalAlloc(GMEM_MOVEABLE, imageSize);
    if (!hMem) return nullptr;
    
    void* pMem = GlobalLock(hMem);
    if (!pMem) {
        GlobalFree(hMem);
        return nullptr;
    }
    
    memcpy(pMem, pResourceData, imageSize);
    GlobalUnlock(hMem);
    
    IStream* pStream = nullptr;
    if (CreateStreamOnHGlobal(hMem, TRUE, &pStream) != S_OK) {
        GlobalFree(hMem);
        return nullptr;
    }
    
    Gdiplus::Image* image = Gdiplus::Image::FromStream(pStream);
    pStream->Release();
    
    return image;
}

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

// 更新养护充电状态
void UpdateBatteryCareStatus() {
    BYTE ecValue = ReadEC(EC_BATTERY_CARE_ADDR);
    bool isEnabled = (ecValue & 0x01) != 0;
    
    SendMessage(g_hBatteryCareCheck, BM_SETCHECK, isEnabled ? BST_CHECKED : BST_UNCHECKED, 0);
    
    if (isEnabled) {
        BYTE level = ReadEC(EC_BATTERY_LEVEL_ADDR);
        if (level > 100 || level < 0) level = 80; // 默认值，范围改为0-100
        
        // 设置编辑框数值
        wchar_t levelText[16];
        swprintf_s(levelText, L"%d", level);
        SetWindowText(g_hBatteryLevelEdit, levelText);
        
        SetWindowText(g_hBatteryLevelLabel, L"充电上限 (0-100%):");
        
        EnableWindow(g_hBatteryLevelEdit, TRUE);
        EnableWindow(g_hBatteryLevelSpin, TRUE);
    } else {
        EnableWindow(g_hBatteryLevelEdit, FALSE);
        EnableWindow(g_hBatteryLevelSpin, FALSE);
        SetWindowText(g_hBatteryLevelLabel, L"充电上限: 已禁用");
    }
}

// 更新性能模式
void UpdatePerformanceMode() {
    BYTE mode = ReadEC(EC_PERFORMANCE_MODE_ADDR);
    int index = 2; // 默认智能模式
    
    switch (mode) {
        case PERF_ECO_MODE: index = 0; break;
        case PERF_QUIET_MODE: index = 1; break;
        case PERF_SMART_MODE: index = 2; break;
        case PERF_FAST_MODE: index = 3; break;
        case PERF_EXTREME_MODE: index = 4; break;
    }
    
    SendMessage(g_hPerformanceCombo, CB_SETCURSEL, index, 0);
}

// 设置养护充电
void SetBatteryCare(bool enable, int level) {
    // 确保范围在0-100之间
    if (level < 0) level = 0;
    if (level > 100) level = 100;
    
    BYTE currentValue = ReadEC(EC_BATTERY_CARE_ADDR);
    if (enable) {
        currentValue |= 0x01;
        WriteEC(EC_BATTERY_LEVEL_ADDR, (BYTE)level);
    } else {
        currentValue &= 0xFE;
    }
    WriteEC(EC_BATTERY_CARE_ADDR, currentValue);
    UpdateBatteryCareStatus();
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
    // 检查控件句柄有效性
    if (!g_hBatteryCareCheck || !g_hBatteryLevelEdit || !g_hPerformanceCombo)
        return;
    std::wofstream ofs(CONFIG_FILE);
    if (!ofs.is_open() || !ofs.good()) return;
    int batteryCare = SendMessage(g_hBatteryCareCheck, BM_GETCHECK, 0, 0) == BST_CHECKED ? 1 : 0;
    wchar_t levelText[16] = {0};
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
    // 应用配置到硬件
    if (batteryCare != -1 && batteryLevel != -1)
        SetBatteryCare(batteryCare != 0, batteryLevel);
    if (perfMode != -1)
        SetPerformanceMode(perfMode);
}

// 创建托盘图标
void CreateTrayIcon() {
    ZeroMemory(&g_nid, sizeof(NOTIFYICONDATA));
    g_nid.cbSize = sizeof(NOTIFYICONDATA);
    g_nid.hWnd = g_hWnd;
    g_nid.uID = 1;
    g_nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    g_nid.uCallbackMessage = WM_TRAYICON;
    
    // 从资源加载托盘图标
    g_nid.hIcon = LoadIcon(g_hInstance, MAKEINTRESOURCE(IDI_TRAY_ICON));
    if (!g_nid.hIcon) {
        // 如果资源加载失败，使用系统默认图标作为后备
        g_nid.hIcon = LoadIcon(nullptr, IDI_APPLICATION);
    }
    
    // 设置提示文本
    wcscpy_s(g_nid.szTip, sizeof(g_nid.szTip) / sizeof(wchar_t), L"小米电脑管家精简版");
    
    // 尝试创建托盘图标
    if (!Shell_NotifyIcon(NIM_ADD, &g_nid)) {
        // 如果失败，显示错误信息
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
    
    // 养护充电子菜单
    HMENU hBatteryMenu = CreatePopupMenu();
    AppendMenu(hBatteryMenu, MF_STRING, ID_TRAY_BATTERY_ENABLE, L"开启养护充电");
    AppendMenu(hBatteryMenu, MF_STRING, ID_TRAY_BATTERY_DISABLE, L"关闭养护充电");
    AppendMenu(hMenu, MF_POPUP, (UINT_PTR)hBatteryMenu, L"养护充电");
    
    // 性能模式子菜单
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
        // 显示窗口
        ShowWindow(g_hWnd, SW_SHOW);
        ShowWindow(g_hWnd, SW_RESTORE);
        SetForegroundWindow(g_hWnd);
        BringWindowToTop(g_hWnd);
        g_isMinimized = false;
    } else {
        // 隐藏窗口
        ShowWindow(g_hWnd, SW_HIDE);
        g_isMinimized = true;
    }
}

// 创建控件
void CreateControls(HWND hwnd) {
    // 创建透明画刷
    g_hTransparentBrush = (HBRUSH)GetStockObject(NULL_BRUSH);
    
    // 标题 - 使用透明背景
    g_hTitleLabel = CreateWindow(L"STATIC", L"小米电脑管家精简版",
        WS_VISIBLE | WS_CHILD | SS_CENTER,
        50, 40, 300, 25, hwnd, nullptr, GetModuleHandle(nullptr), nullptr);
    
    // 养护充电复选框 - 设置透明背景
    g_hBatteryCareCheck = CreateWindow(L"BUTTON", L"开启养护充电",
        WS_VISIBLE | WS_CHILD | BS_AUTOCHECKBOX,
        30, 80, 150, 25, hwnd, (HMENU)ID_BATTERY_CARE_ENABLE, GetModuleHandle(nullptr), nullptr);
    
    // 充电上限标签 - 使用透明背景
    g_hBatteryLevelLabel = CreateWindow(L"STATIC", L"充电上限 (0-100%):",
        WS_VISIBLE | WS_CHILD,
        30, 115, 150, 20, hwnd, nullptr, GetModuleHandle(nullptr), nullptr);
    
    // 充电上限数值输入框
    g_hBatteryLevelEdit = CreateWindow(L"EDIT", L"80",
        WS_VISIBLE | WS_CHILD | WS_BORDER | ES_NUMBER | ES_RIGHT,
        30, 140, 60, 25, hwnd, (HMENU)ID_BATTERY_LEVEL_EDIT, GetModuleHandle(nullptr), nullptr);
    
    // 数值调节器（上下调节按钮）
    g_hBatteryLevelSpin = CreateWindow(UPDOWN_CLASS, L"",
        WS_VISIBLE | WS_CHILD | UDS_SETBUDDYINT | UDS_ALIGNRIGHT | UDS_ARROWKEYS | UDS_NOTHOUSANDS,
        90, 140, 20, 25, hwnd, (HMENU)ID_BATTERY_LEVEL_SPIN, GetModuleHandle(nullptr), nullptr);
    
    // 设置数值调节器的关联控件和范围
    SendMessage(g_hBatteryLevelSpin, UDM_SETBUDDY, (WPARAM)g_hBatteryLevelEdit, 0);
    SendMessage(g_hBatteryLevelSpin, UDM_SETRANGE32, 0, 100);
    SendMessage(g_hBatteryLevelSpin, UDM_SETPOS32, 0, 80);
    
    // 百分号标签
    CreateWindow(L"STATIC", L"%",
        WS_VISIBLE | WS_CHILD,
        115, 145, 15, 20, hwnd, nullptr, GetModuleHandle(nullptr), nullptr);
    
    // 性能模式标签 - 使用透明背景
    g_hPerfLabel = CreateWindow(L"STATIC", L"性能模式:",
        WS_VISIBLE | WS_CHILD,
        30, 200, 80, 20, hwnd, nullptr, GetModuleHandle(nullptr), nullptr);
    
    // 性能模式下拉框
    g_hPerformanceCombo = CreateWindow(WC_COMBOBOX, L"",
        WS_VISIBLE | WS_CHILD | CBS_DROPDOWNLIST | CBS_HASSTRINGS,
        120, 195, 150, 200, hwnd, (HMENU)ID_PERFORMANCE_MODE, GetModuleHandle(nullptr), nullptr);
    
    // 添加性能模式选项
    SendMessage(g_hPerformanceCombo, CB_ADDSTRING, 0, (LPARAM)L"省电模式");
    SendMessage(g_hPerformanceCombo, CB_ADDSTRING, 0, (LPARAM)L"静谧模式");
    SendMessage(g_hPerformanceCombo, CB_ADDSTRING, 0, (LPARAM)L"智能模式");
    SendMessage(g_hPerformanceCombo, CB_ADDSTRING, 0, (LPARAM)L"极速模式");
    SendMessage(g_hPerformanceCombo, CB_ADDSTRING, 0, (LPARAM)L"狂暴模式");
    SendMessage(g_hPerformanceCombo, CB_SETCURSEL, 2, 0); // 默认智能模式
    
    // 最小化按钮 - 使用自定义样式
    CreateWindow(L"BUTTON", L"—",
        WS_VISIBLE | WS_CHILD | BS_PUSHBUTTON | BS_FLAT,
        320, 10, 30, 25, hwnd, (HMENU)ID_MIN_BUTTON, GetModuleHandle(nullptr), nullptr);
    
    // 关闭按钮 - 使用自定义样式
    CreateWindow(L"BUTTON", L"×",
        WS_VISIBLE | WS_CHILD | BS_PUSHBUTTON | BS_FLAT,
        355, 10, 30, 25, hwnd, (HMENU)ID_CLOSE_BUTTON, GetModuleHandle(nullptr), nullptr);
    
    // 刷新按钮
    HWND hRefreshBtn = CreateWindow(L"BUTTON", L"刷新状态",
        WS_VISIBLE | WS_CHILD | BS_PUSHBUTTON,
        30, 240, 100, 30, hwnd, (HMENU)ID_REFRESH_BUTTON, GetModuleHandle(nullptr), nullptr);

    // 开机自启动复选框
    HWND hAutoStartCheck = CreateWindow(L"BUTTON", L"开机自启动",
        WS_VISIBLE | WS_CHILD | BS_AUTOCHECKBOX,
        140, 245, 120, 22, hwnd, (HMENU)ID_AUTO_START_CHECK, GetModuleHandle(nullptr), nullptr);
    // 设置初始状态
    SendMessage(hAutoStartCheck, BM_SETCHECK, IsAutoStartEnabled() ? BST_CHECKED : BST_UNCHECKED, 0);
}

// 检查图像是否有透明度
bool HasImageTransparency(Gdiplus::Image* image) {
    if (!image) return false;
    
    // 检查像素格式是否支持Alpha通道
    Gdiplus::PixelFormat format = image->GetPixelFormat();
    return (format & PixelFormatAlpha) != 0 || 
           (format & PixelFormatPAlpha) != 0 ||
           format == PixelFormat32bppARGB ||
           format == PixelFormat64bppARGB ||
           format == PixelFormat16bppARGB1555;
}

// 绘制背景
void DrawBackground(HDC hdc, RECT* rect) {
    if (g_backgroundImage && g_backgroundImage->GetLastStatus() == Gdiplus::Ok) {
        Gdiplus::Graphics graphics(hdc);
        
        // 设置高质量渲染
        graphics.SetInterpolationMode(Gdiplus::InterpolationModeHighQualityBicubic);
        graphics.SetSmoothingMode(Gdiplus::SmoothingModeHighQuality);
        graphics.SetCompositingMode(Gdiplus::CompositingModeSourceOver);
        graphics.SetCompositingQuality(Gdiplus::CompositingQualityHighQuality);
        
        // 先清除背景为白色
        graphics.Clear(Gdiplus::Color(255, 255, 255, 255));
        
        // 绘制背景图像
        graphics.DrawImage(g_backgroundImage, 0, 0, rect->right, rect->bottom);
    } else {
        // 如果没有背景图，使用渐变色背景
        HBRUSH hBrush = CreateSolidBrush(RGB(245, 250, 255));
        FillRect(hdc, rect, hBrush);
        DeleteObject(hBrush);
        
        // 添加边框
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
}

// 创建程序化渐变背景
void CreateProgrammaticBackground() {
    const int width = 400;
    const int height = 280;
    
    Gdiplus::Bitmap* bitmap = new Gdiplus::Bitmap(width, height, PixelFormat24bppRGB);
    Gdiplus::Graphics graphics(bitmap);
    
    // 创建线性渐变
    Gdiplus::LinearGradientBrush brush(
        Gdiplus::Point(0, 0),
        Gdiplus::Point(0, height),
        Gdiplus::Color(245, 250, 255), // 浅蓝色
        Gdiplus::Color(220, 235, 255)  // 更深的蓝色
    );
    
    graphics.FillRectangle(&brush, 0, 0, width, height);
    
    // 添加一些装饰性元素
    Gdiplus::Pen pen(Gdiplus::Color(180, 200, 255), 2);
    graphics.DrawRectangle(&pen, 5, 5, width - 10, height - 10);
    
    // 添加标题区域装饰
    Gdiplus::LinearGradientBrush titleBrush(
        Gdiplus::Point(0, 10),
        Gdiplus::Point(0, 60),
        Gdiplus::Color(200, 220, 240, 255),
        Gdiplus::Color(240, 245, 255, 255)
    );
    graphics.FillRectangle(&titleBrush, 10, 10, width - 20, 50);
    
    g_backgroundImage = bitmap;
}

// 加载背景图像
void LoadBackgroundImage() {
    // 从资源中加载背景图
    g_backgroundImage = LoadImageFromResource(g_hInstance, IDB_BACKGROUND, RT_RCDATA);
    if (!g_backgroundImage || g_backgroundImage->GetLastStatus() != Gdiplus::Ok) {
        // 如果资源加载失败，创建一个程序化的后备背景
        CreateProgrammaticBackground();
    }
}

// 注册默认热键
void RegisterDefaultHotKeys(HWND hwnd) {
    // Ctrl+Alt+B 用于开启/关闭养护充电
    RegisterHotKey(hwnd, HOTKEY_ID_BATTERY, MOD_CONTROL | MOD_ALT, 'B');
    // Ctrl+Alt+P 用于循环切换性能模式
    RegisterHotKey(hwnd, HOTKEY_ID_PERF, MOD_CONTROL | MOD_ALT, 'P');
}

// 注销热键
void UnregisterCustomHotKeys(HWND hwnd) {
    UnregisterHotKey(hwnd, HOTKEY_ID_BATTERY);
    UnregisterHotKey(hwnd, HOTKEY_ID_PERF);
}

// 窗口过程
LRESULT CALLBACK WindowProc(HWND hwnd, UINT uMsg, WPARAM wParam, LPARAM lParam) {
    switch (uMsg) {
        case WM_CREATE:
            CreateControls(hwnd);
            RegisterDefaultHotKeys(hwnd); // 注册默认热键
            if (InitializeWinRing0()) {
                LoadConfig(); // 初始化成功后加载配置并应用
            } else {
                // 初始化失败，仅加载配置到UI，不应用到硬件
                LoadConfig(); 
            }
            UpdateBatteryCareStatus(); // 更新UI状态
            UpdatePerformanceMode();   // 更新UI状态
            
            SetTimer(hwnd, 1, 1000, nullptr); // 1秒后创建托盘图标

            if (IsAutoStartEnabled()) {
                ShowWindow(hwnd, SW_HIDE);
                g_isMinimized = true;
            }
            break;
            
        case WM_POWERBROADCAST:
            if (wParam == PBT_APMPOWERSTATUSCHANGE) {
                // 电源状态改变，3秒后恢复性能模式
                SetTimer(hwnd, 100, 3000, NULL);
            }
            break;
            
        case WM_TIMER:
            if (wParam == 1) {
                KillTimer(hwnd, 1);
                CreateTrayIcon();
            } else if (wParam == 100) {
                KillTimer(hwnd, 100);
                // 3秒后恢复所有设置
                // 恢复养护充电
                bool careChecked = SendMessage(g_hBatteryCareCheck, BM_GETCHECK, 0, 0) == BST_CHECKED;
                wchar_t levelText[16];
                GetWindowText(g_hBatteryLevelEdit, levelText, 16);
                int careLevel = _wtoi(levelText);
                if (careLevel < 0) careLevel = 0;
                if (careLevel > 100) careLevel = 100;
                SetBatteryCare(careChecked, careLevel);
                // 恢复性能模式
                int sel = (int)SendMessage(g_hPerformanceCombo, CB_GETCURSEL, 0, 0);
                SetPerformanceMode(sel);
            }
            break;
            
        case WM_PAINT: {
            PAINTSTRUCT ps;
            HDC hdc = BeginPaint(hwnd, &ps);
            
            RECT rect;
            GetClientRect(hwnd, &rect);
            
            // 始终使用正常绘制，简化逻辑
            DrawBackground(hdc, &rect);
            
            EndPaint(hwnd, &ps);
            break;
        }
        
        case WM_ERASEBKGND:
            // 阻止默认背景擦除，我们自己处理
            return 1;
            
        // 恢复窗口拖动逻辑
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
            
        // 让所有静态控件文字背景透明
        case WM_CTLCOLORSTATIC: {
            HDC hdcStatic = (HDC)wParam;
            SetBkMode(hdcStatic, TRANSPARENT);
            SetTextColor(hdcStatic, RGB(51, 51, 51)); // 深灰色
            return (LRESULT)GetStockObject(NULL_BRUSH);
        }
        
        case WM_COMMAND:
            switch (LOWORD(wParam)) {
                case ID_BATTERY_CARE_ENABLE:
                case ID_BATTERY_LEVEL_EDIT:
                case ID_PERFORMANCE_MODE:
                    switch (LOWORD(wParam)) {
                        case ID_BATTERY_CARE_ENABLE: {
                            bool isChecked = SendMessage(g_hBatteryCareCheck, BM_GETCHECK, 0, 0) == BST_CHECKED;
                            
                            // 获取当前输入框的数值
                            wchar_t text[16];
                            GetWindowText(g_hBatteryLevelEdit, text, 16);
                            int level = _wtoi(text);
                            if (level < 0) level = 0;
                            if (level > 100) level = 100;
                            
                            SetBatteryCare(isChecked, level);
                            
                            // 强制重绘复选框区域以更新透明背景
                            InvalidateRect(g_hBatteryCareCheck, nullptr, TRUE);
                            break;
                        }
                        case ID_BATTERY_LEVEL_EDIT: {
                            if (HIWORD(wParam) == EN_CHANGE) {
                                // 当输入框内容改变时，更新EC数据（如果养护充电已开启）
                                if (SendMessage(g_hBatteryCareCheck, BM_GETCHECK, 0, 0) == BST_CHECKED) {
                                    wchar_t text[16];
                                    GetWindowText(g_hBatteryLevelEdit, text, 16);
                                    int level = _wtoi(text);
                                    if (level >= 0 && level <= 100) {
                                        WriteEC(EC_BATTERY_LEVEL_ADDR, (BYTE)level);
                                    }
                                }
                            }
                            break;
                        }
                        case ID_PERFORMANCE_MODE: {
                            if (HIWORD(wParam) == CBN_SELCHANGE) {
                                int sel = (int)SendMessage(g_hPerformanceCombo, CB_GETCURSEL, 0, 0);
                                SetPerformanceMode(sel);
                            }
                            break;
                        }
                    }
                    break;
                case ID_MIN_BUTTON:
                    ShowWindow(hwnd, SW_HIDE);
                    g_isMinimized = true;
                    // 如果托盘图标还未创建，现在创建
                    if (g_nid.hWnd == nullptr) {
                        CreateTrayIcon();
                    } else {
                        // 确保托盘图标可见
                        Shell_NotifyIcon(NIM_MODIFY, &g_nid);
                    }
                    break;
                case ID_CLOSE_BUTTON:
                    DestroyWindow(hwnd);
                    break;
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
                case ID_AUTO_START_CHECK: {
                    bool checked = (SendMessage((HWND)lParam, BM_GETCHECK, 0, 0) == BST_CHECKED);
                    SetAutoStart(checked);
                    break;
                }
                case ID_REFRESH_BUTTON:
                    UpdateBatteryCareStatus();
                    UpdatePerformanceMode();
                    break;
            }
            break;
            
        case WM_TRAYICON:
            switch (lParam) {
                case WM_LBUTTONDBLCLK:
                case WM_LBUTTONUP: // 添加单击支持
                    ToggleMainWindow();
                    break;
                case WM_RBUTTONUP:
                case WM_CONTEXTMENU:
                    ShowTrayMenu();
                    break;
            }
            break;
            
        case WM_HOTKEY:
            if (wParam == HOTKEY_ID_BATTERY) {
                // 切换养护充电
                bool isChecked = SendMessage(g_hBatteryCareCheck, BM_GETCHECK, 0, 0) == BST_CHECKED;
                wchar_t text[16];
                GetWindowText(g_hBatteryLevelEdit, text, 16);
                int level = _wtoi(text);
                if (level < 0) level = 0;
                if (level > 100) level = 100;
                SetBatteryCare(!isChecked, level);
            } else if (wParam == HOTKEY_ID_PERF) {
                // 切换到自定义性能模式，再次切回原模式
                int current = (int)SendMessage(g_hPerformanceCombo, CB_GETCURSEL, 0, 0);
                if (current != g_customPerfModeIndex) {
                    g_lastPerfModeIndex = current;
                    g_perfModeIndex = g_customPerfModeIndex;
                } else {
                    g_perfModeIndex = g_lastPerfModeIndex;
                }
                SetPerformanceMode(g_perfModeIndex);
                SendMessage(g_hPerformanceCombo, CB_SETCURSEL, g_perfModeIndex, 0);
            }
            break;
            
        case WM_DESTROY:
            SaveConfig(); // 退出时保存
            RemoveTrayIcon();
            UnregisterCustomHotKeys(hwnd);
            DeinitializeWinRing0();
            if (g_backgroundImage) {
                delete g_backgroundImage;
                g_backgroundImage = nullptr;
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
    g_hInstance = hInstance; // 保存实例句柄

    // 初始化COM
    CoInitializeEx(NULL, COINIT_MULTITHREADED);

    // 设置当前目录为程序所在目录，保证配置文件读写正确
    wchar_t exePath[MAX_PATH];
    GetModuleFileNameW(nullptr, exePath, MAX_PATH);
    wchar_t* lastSlash = wcsrchr(exePath, L'\\');
    if (lastSlash) {
        *lastSlash = L'\0';
        SetCurrentDirectoryW(exePath);
    }

    // 初始化GDI+
    Gdiplus::GdiplusStartupInput gdiplusStartupInput;
    ULONG_PTR gdiplusToken;
    Gdiplus::GdiplusStartup(&gdiplusToken, &gdiplusStartupInput, nullptr);

    // 初始化通用控件
    INITCOMMONCONTROLSEX icex;
    icex.dwSize = sizeof(INITCOMMONCONTROLSEX);
    icex.dwICC = ICC_STANDARD_CLASSES | ICC_BAR_CLASSES;
    InitCommonControlsEx(&icex);

    // 注册窗口类
    WNDCLASS wc = {};
    wc.lpfnWndProc = WindowProc;
    wc.hInstance = hInstance;
    wc.lpszClassName = CLASS_NAME;
    wc.hbrBackground = nullptr;
    wc.hCursor = LoadCursor(nullptr, IDC_ARROW);

    // 从资源加载主窗口图标
    wc.hIcon = LoadIcon(hInstance, MAKEINTRESOURCE(IDI_MAIN_ICON));
    if (!wc.hIcon) {
        // 如果资源加载失败，使用系统默认图标作为后备
        wc.hIcon = LoadIcon(nullptr, IDI_APPLICATION);
    }

    if (!RegisterClass(&wc)) {
        MessageBox(nullptr, L"注册窗口类失败", L"错误", MB_OK | MB_ICONERROR);
        return 0;
    }

    // 加载背景图像
    LoadBackgroundImage();

    // 创建窗口
    DWORD exStyle = 0;
    g_hWnd = CreateWindowEx(
        exStyle,
        CLASS_NAME,
        WINDOW_TITLE,
        WS_POPUP | WS_VISIBLE,
        (GetSystemMetrics(SM_CXSCREEN) - 400) / 2, // 居中显示
        (GetSystemMetrics(SM_CYSCREEN) - 280) / 2,
        400, 280,
        nullptr, nullptr, hInstance, nullptr
    );

    if (g_hWnd == nullptr) {
        MessageBox(nullptr, L"创建窗口失败", L"错误", MB_OK | MB_ICONERROR);
        return 0;
    }

    // 设置窗口半透明
    SetWindowLongPtr(g_hWnd, GWL_EXSTYLE, GetWindowLongPtr(g_hWnd, GWL_EXSTYLE) | WS_EX_LAYERED);
    SetLayeredWindowAttributes(g_hWnd, 0, 180, LWA_ALPHA);

    // 检查是否自启动，若是则最小化
    if (IsAutoStartEnabled() && wcslen(lpCmdLine) == 0) {
        ShowWindow(g_hWnd, SW_HIDE);
        ((NOTIFYICONDATA*)&g_nid)->hWnd = nullptr; // 防止重复创建托盘
        g_isMinimized = true;
    } else {
        ShowWindow(g_hWnd, nCmdShow);
    }

    UpdateWindow(g_hWnd);

    // 强制重绘以确保背景正确显示
    InvalidateRect(g_hWnd, nullptr, TRUE);

    // 消息循环
    MSG msg = {};
    while (GetMessage(&msg, nullptr, 0, 0)) {
        TranslateMessage(&msg);
        DispatchMessage(&msg);
    }

    // 清理GDI+
    Gdiplus::GdiplusShutdown(gdiplusToken);

    // 反初始化COM
    CoUninitialize();

    return (int)msg.wParam;
}