#[derive(Debug, thiserror::Error)]
pub enum EcError {
    #[error("WinRing0 DLL 加载失败: {0}")]
    DllLoad(String),
    #[error("WinRing0 初始化失败")]
    InitFailed,
    #[error("EC I/O 超时 (地址: {0:#x})")]
    IoTimeout(u16),
    #[error("WMI 连接失败: {0}")]
    WmiConnect(String),
    #[error("WMI MICommonInterface 未找到")]
    WmiInterfaceNotFound,
    #[error("WMI MiInterface 调用失败 (状态={0})")]
    WmiCallFailed(u16),
    #[error("EC 读取失败 (地址: {0:#x})")]
    ReadFailed(u16),
    #[error("EC 写入失败 (地址: {0:#x})")]
    WriteFailed(u16),
    #[error("后端不可用: {0}")]
    BackendUnavailable(String),
    #[error("不支持的后端类型")]
    UnsupportedBackend,
}
