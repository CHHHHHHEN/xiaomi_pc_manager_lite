#[derive(Debug, Clone, thiserror::Error)]
pub enum EcError {
    #[error("WinRing0 DLL 加载失败: {0}")]
    DllLoad(String),
    #[error("WinRing0 初始化失败: {0}")]
    InitFailed(String),
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
    #[error("EC 操作超时 (地址: {0:#x})")]
    Timeout(u16),
    #[error("后端不可用: {0}")]
    BackendUnavailable(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_dll_load() {
        let err = EcError::DllLoad("无法加载 DLL".into());
        assert_eq!(err.to_string(), "WinRing0 DLL 加载失败: 无法加载 DLL");
    }

    #[test]
    fn test_display_init_failed() {
        let err = EcError::InitFailed("拒绝访问 (0x5)".into());
        assert_eq!(err.to_string(), "WinRing0 初始化失败: 拒绝访问 (0x5)");
    }

    #[test]
    fn test_display_wmi_connect() {
        let err = EcError::WmiConnect("拒绝访问".into());
        assert_eq!(err.to_string(), "WMI 连接失败: 拒绝访问");
    }

    #[test]
    fn test_display_wmi_interface_not_found() {
        let err = EcError::WmiInterfaceNotFound;
        assert_eq!(err.to_string(), "WMI MICommonInterface 未找到");
    }

    #[test]
    fn test_display_wmi_call_failed() {
        let err = EcError::WmiCallFailed(0x0001);
        assert_eq!(err.to_string(), "WMI MiInterface 调用失败 (状态=1)");
    }

    #[test]
    fn test_display_timeout() {
        let err = EcError::Timeout(0x66);
        assert_eq!(err.to_string(), "EC 操作超时 (地址: 0x66)");
    }

    #[test]
    fn test_display_read_failed() {
        let err = EcError::ReadFailed(0xA4);
        assert_eq!(err.to_string(), "EC 读取失败 (地址: 0xa4)");
    }

    #[test]
    fn test_display_write_failed() {
        let err = EcError::WriteFailed(0x68);
        assert_eq!(err.to_string(), "EC 写入失败 (地址: 0x68)");
    }

    #[test]
    fn test_display_backend_unavailable() {
        let err = EcError::BackendUnavailable("两个后端均不可用".into());
        assert_eq!(err.to_string(), "后端不可用: 两个后端均不可用");
    }

    #[test]
    fn test_error_trait_impl() {
        fn assert_error<E: std::error::Error>() {}
        assert_error::<EcError>();
    }

    #[test]
    fn test_debug_impl() {
        let err = EcError::WmiCallFailed(0x0001);
        let debug = format!("{:?}", err);
        assert!(debug.contains("WmiCallFailed"));
    }

    #[test]
    fn test_source_returns_none() {
        use std::error::Error;
        let err = EcError::InitFailed("error".into());
        assert!(err.source().is_none());
        let err = EcError::DllLoad("foo".into());
        assert!(err.source().is_none());
    }
}
