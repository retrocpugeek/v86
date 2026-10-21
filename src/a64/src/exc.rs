//! Synchronous exceptions raised while executing an instruction.

pub mod ec {
    pub const UNKNOWN: u32 = 0x00;
    pub const WFX: u32 = 0x01;
    pub const FP_ACCESS: u32 = 0x07;
    pub const ILLEGAL_STATE: u32 = 0x0e;
    pub const SVC64: u32 = 0x15;
    pub const HVC64: u32 = 0x16;
    pub const SMC64: u32 = 0x17;
    pub const SYSREG: u32 = 0x18;
    pub const IABT_LOWER: u32 = 0x20;
    pub const IABT_CUR: u32 = 0x21;
    pub const PC_ALIGN: u32 = 0x22;
    pub const DABT_LOWER: u32 = 0x24;
    pub const DABT_CUR: u32 = 0x25;
    pub const SP_ALIGN: u32 = 0x26;
    pub const SOFTSTEP_LOWER: u32 = 0x32;
    pub const BRK64: u32 = 0x3c;
}

/// Fault status codes (DFSC/IFSC); the level is added for the table-walk ones.
pub mod fsc {
    pub const ADDR_SIZE: u32 = 0x00;
    pub const TRANSLATION: u32 = 0x04;
    pub const ACCESS_FLAG: u32 = 0x08;
    pub const PERMISSION: u32 = 0x0c;
    pub const ALIGNMENT: u32 = 0x21;
}

#[derive(Clone, Copy, Debug)]
pub struct Exception {
    pub ec: u32,
    pub iss: u32,
    pub far: Option<u64>,
    /// Preferred return address is the next instruction (SVC/HVC/SMC).
    pub ret_next: bool,
}

pub type Res<T> = Result<T, Exception>;

impl Exception {
    pub fn undef() -> Exception { Exception { ec: ec::UNKNOWN, iss: 0, far: None, ret_next: false } }

    pub fn new(ec: u32, iss: u32) -> Exception { Exception { ec, iss, far: None, ret_next: false } }
}

#[inline(always)]
pub fn undef<T>() -> Res<T> { Err(Exception::undef()) }
