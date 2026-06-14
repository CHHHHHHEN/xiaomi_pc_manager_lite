use super::backend::EcBackend;
use super::error::EcError;
use libloading::Library;


type ReadPort = unsafe extern "system" fn(u16) -> u8;
type WritePort = unsafe extern "system" fn(u16, u8);

const EC_DATA: u16 = 0x62;
const EC_CMD: u16 = 0x66;

fn ec_wait_write(rp: ReadPort) {
    for _ in 0..1000 {
        if unsafe { rp(EC_CMD) } & 0x02 == 0 {
            break;
        }
    }
}

fn ec_wait_read(rp: ReadPort) {
    for _ in 0..1000 {
        if unsafe { rp(EC_CMD) } & 0x01 == 0 {
            break;
        }
    }
}

pub struct WinRing0Backend {
    _lib: Library,
    rp: ReadPort,
    wp: WritePort,
}

unsafe impl Send for WinRing0Backend {}
unsafe impl Sync for WinRing0Backend {}

impl WinRing0Backend {
    pub fn new() -> Result<Self, EcError> {
        let dll_name = if cfg!(target_pointer_width = "64") {
            "WinRing0x64.dll"
        } else {
            "WinRing0.dll"
        };

        let lib = unsafe { Library::new(dll_name) }
            .map_err(|e| EcError::DllLoad(e.to_string()))?;

        let init: unsafe extern "system" fn() -> i32 =
            *unsafe { lib.get(b"InitializeOls") }
                .map_err(|e| EcError::DllLoad(e.to_string()))?;

        if unsafe { init() } != 0 {
            return Err(EcError::InitFailed);
        }

        let rp: ReadPort = *unsafe { lib.get(b"ReadIoPortByte") }
            .map_err(|e| EcError::DllLoad(e.to_string()))?;

        let wp: WritePort = *unsafe { lib.get(b"WriteIoPortByte") }
            .map_err(|e| EcError::DllLoad(e.to_string()))?;

        Ok(Self { _lib: lib, rp, wp })
    }
}

impl EcBackend for WinRing0Backend {
    fn name(&self) -> &'static str {
        "WinRing0 (I/O Port)"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn read_byte(&self, addr: u16) -> Result<u8, EcError> {
        unsafe { (self.wp)(EC_CMD, 0x80) };
        ec_wait_write(self.rp);
        unsafe { (self.wp)(EC_DATA, addr as u8) };
        ec_wait_write(self.rp);
        unsafe { (self.wp)(EC_CMD, 0x80) };
        ec_wait_read(self.rp);
        Ok(unsafe { (self.rp)(EC_DATA) })
    }

    fn write_byte(&self, addr: u16, value: u8) -> Result<(), EcError> {
        unsafe { (self.wp)(EC_CMD, 0x81) };
        ec_wait_write(self.rp);
        unsafe { (self.wp)(EC_DATA, addr as u8) };
        ec_wait_write(self.rp);
        unsafe { (self.wp)(EC_DATA, value) };
        ec_wait_write(self.rp);
        Ok(())
    }

    // ── High-level battery ──

    fn get_battery_care_enabled(&self) -> Result<bool, EcError> {
        let val = self.read_byte(0xA4)?;
        Ok(val == 0x01)
    }

    fn get_charge_limit(&self) -> Result<u8, EcError> {
        self.read_byte(0xA7)
    }

    fn set_battery_care(&self, enabled: bool) -> Result<(), EcError> {
        self.write_byte(0xA4, if enabled { 0x01 } else { 0x00 })
    }

    fn set_charge_limit(&self, percent: u8) -> Result<(), EcError> {
        self.write_byte(0xA7, percent.min(100))
    }

    // ── High-level performance mode ──

    fn get_performance_mode(&self) -> Result<u8, EcError> {
        self.read_byte(0x68)
    }

    fn set_performance_mode(&self, mode: u8) -> Result<(), EcError> {
        self.write_byte(0x68, mode)
    }
}
