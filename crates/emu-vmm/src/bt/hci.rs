//! HCI codes (Bluetooth Core 5.x, Vol 4 Part E) and the event encoders the controller uses.
//! Multi-byte fields are little-endian; a BD_ADDR is six bytes, least significant first.

/// Command opcodes (OGF << 10 | OCF).
pub mod op {
    pub const INQUIRY: u16 = 0x0401;
    pub const INQUIRY_CANCEL: u16 = 0x0402;
    pub const CREATE_CONNECTION: u16 = 0x0405;
    pub const DISCONNECT: u16 = 0x0406;
    pub const CREATE_CONNECTION_CANCEL: u16 = 0x0408;
    pub const ACCEPT_CONNECTION_REQUEST: u16 = 0x0409;
    pub const REJECT_CONNECTION_REQUEST: u16 = 0x040A;
    pub const LINK_KEY_REQUEST_REPLY: u16 = 0x040B;
    pub const LINK_KEY_REQUEST_NEGATIVE_REPLY: u16 = 0x040C;
    pub const PIN_CODE_REQUEST_REPLY: u16 = 0x040D;
    pub const PIN_CODE_REQUEST_NEGATIVE_REPLY: u16 = 0x040E;
    pub const REMOTE_NAME_REQUEST: u16 = 0x0419;
    pub const REMOTE_NAME_REQUEST_CANCEL: u16 = 0x041A;
    pub const READ_DEFAULT_LINK_POLICY_SETTINGS: u16 = 0x080E;
    pub const WRITE_DEFAULT_LINK_POLICY_SETTINGS: u16 = 0x080F;
    pub const SET_EVENT_MASK: u16 = 0x0C01;
    pub const RESET: u16 = 0x0C03;
    pub const SET_EVENT_FILTER: u16 = 0x0C05;
    pub const READ_STORED_LINK_KEY: u16 = 0x0C0D;
    pub const DELETE_STORED_LINK_KEY: u16 = 0x0C12;
    pub const WRITE_LOCAL_NAME: u16 = 0x0C13;
    pub const READ_LOCAL_NAME: u16 = 0x0C14;
    pub const READ_CONNECTION_ACCEPT_TIMEOUT: u16 = 0x0C15;
    pub const WRITE_CONNECTION_ACCEPT_TIMEOUT: u16 = 0x0C16;
    pub const READ_PAGE_TIMEOUT: u16 = 0x0C17;
    pub const WRITE_PAGE_TIMEOUT: u16 = 0x0C18;
    pub const READ_SCAN_ENABLE: u16 = 0x0C19;
    pub const WRITE_SCAN_ENABLE: u16 = 0x0C1A;
    pub const READ_PAGE_SCAN_ACTIVITY: u16 = 0x0C1B;
    pub const WRITE_PAGE_SCAN_ACTIVITY: u16 = 0x0C1C;
    pub const READ_INQUIRY_SCAN_ACTIVITY: u16 = 0x0C1D;
    pub const WRITE_INQUIRY_SCAN_ACTIVITY: u16 = 0x0C1E;
    pub const READ_AUTHENTICATION_ENABLE: u16 = 0x0C1F;
    pub const WRITE_AUTHENTICATION_ENABLE: u16 = 0x0C20;
    pub const READ_CLASS_OF_DEVICE: u16 = 0x0C23;
    pub const WRITE_CLASS_OF_DEVICE: u16 = 0x0C24;
    pub const READ_VOICE_SETTING: u16 = 0x0C25;
    pub const WRITE_VOICE_SETTING: u16 = 0x0C26;
    pub const SET_CONTROLLER_TO_HOST_FLOW_CONTROL: u16 = 0x0C31;
    pub const HOST_BUFFER_SIZE: u16 = 0x0C33;
    pub const READ_NUMBER_OF_SUPPORTED_IAC: u16 = 0x0C38;
    pub const READ_CURRENT_IAC_LAP: u16 = 0x0C39;
    pub const WRITE_CURRENT_IAC_LAP: u16 = 0x0C3A;
    pub const READ_INQUIRY_SCAN_TYPE: u16 = 0x0C42;
    pub const WRITE_INQUIRY_SCAN_TYPE: u16 = 0x0C43;
    pub const READ_INQUIRY_MODE: u16 = 0x0C44;
    pub const WRITE_INQUIRY_MODE: u16 = 0x0C45;
    pub const READ_PAGE_SCAN_TYPE: u16 = 0x0C46;
    pub const WRITE_PAGE_SCAN_TYPE: u16 = 0x0C47;
    pub const READ_EXTENDED_INQUIRY_RESPONSE: u16 = 0x0C51;
    pub const WRITE_EXTENDED_INQUIRY_RESPONSE: u16 = 0x0C52;
    pub const READ_SIMPLE_PAIRING_MODE: u16 = 0x0C55;
    pub const WRITE_SIMPLE_PAIRING_MODE: u16 = 0x0C56;
    pub const READ_INQUIRY_RESPONSE_TRANSMIT_POWER_LEVEL: u16 = 0x0C58;
    pub const READ_DEFAULT_ERRONEOUS_DATA_REPORTING: u16 = 0x0C5A;
    pub const WRITE_DEFAULT_ERRONEOUS_DATA_REPORTING: u16 = 0x0C5B;
    pub const SET_EVENT_MASK_PAGE_2: u16 = 0x0C63;
    pub const READ_LE_HOST_SUPPORT: u16 = 0x0C6C;
    pub const WRITE_LE_HOST_SUPPORT: u16 = 0x0C6D;
    pub const READ_LOCAL_VERSION_INFORMATION: u16 = 0x1001;
    pub const READ_LOCAL_SUPPORTED_COMMANDS: u16 = 0x1002;
    pub const READ_LOCAL_SUPPORTED_FEATURES: u16 = 0x1003;
    pub const READ_LOCAL_EXTENDED_FEATURES: u16 = 0x1004;
    pub const READ_BUFFER_SIZE: u16 = 0x1005;
    pub const READ_BD_ADDR: u16 = 0x1009;
    pub const LE_SET_EVENT_MASK: u16 = 0x2001;
    pub const LE_READ_BUFFER_SIZE: u16 = 0x2002;
    pub const LE_READ_LOCAL_SUPPORTED_FEATURES: u16 = 0x2003;
    pub const LE_SET_RANDOM_ADDRESS: u16 = 0x2005;
    pub const LE_SET_ADVERTISING_PARAMETERS: u16 = 0x2006;
    pub const LE_READ_ADVERTISING_PHYSICAL_CHANNEL_TX_POWER: u16 = 0x2007;
    pub const LE_SET_ADVERTISING_DATA: u16 = 0x2008;
    pub const LE_SET_SCAN_RESPONSE_DATA: u16 = 0x2009;
    pub const LE_SET_ADVERTISING_ENABLE: u16 = 0x200A;
    pub const LE_SET_SCAN_PARAMETERS: u16 = 0x200B;
    pub const LE_SET_SCAN_ENABLE: u16 = 0x200C;
    pub const LE_CREATE_CONNECTION: u16 = 0x200D;
    pub const LE_CREATE_CONNECTION_CANCEL: u16 = 0x200E;
    pub const LE_READ_FILTER_ACCEPT_LIST_SIZE: u16 = 0x200F;
    pub const LE_CLEAR_FILTER_ACCEPT_LIST: u16 = 0x2010;
    pub const LE_ADD_DEVICE_TO_FILTER_ACCEPT_LIST: u16 = 0x2011;
    pub const LE_REMOVE_DEVICE_FROM_FILTER_ACCEPT_LIST: u16 = 0x2012;
    pub const LE_RAND: u16 = 0x2018;
    pub const LE_READ_SUPPORTED_STATES: u16 = 0x201C;

    /// The vendor-specific group (OGF 0x3F), e.g. Android's LE Get Vendor Capabilities (0xFD53).
    pub fn is_vendor(op: u16) -> bool {
        op >> 10 == 0x3f
    }
}

/// Event codes.
pub mod ev {
    pub const INQUIRY_COMPLETE: u8 = 0x01;
    pub const CONNECTION_COMPLETE: u8 = 0x03;
    pub const REMOTE_NAME_REQUEST_COMPLETE: u8 = 0x07;
    pub const COMMAND_COMPLETE: u8 = 0x0E;
    pub const COMMAND_STATUS: u8 = 0x0F;
    pub const LE_META: u8 = 0x3E;
    /// LE Meta subevent: LE Connection Complete.
    pub const LE_CONNECTION_COMPLETE: u8 = 0x01;
}

/// Error codes (Vol 1 Part F).
pub mod status {
    pub const SUCCESS: u8 = 0x00;
    pub const UNKNOWN_COMMAND: u8 = 0x01;
    pub const UNKNOWN_CONNECTION: u8 = 0x02;
    pub const PAGE_TIMEOUT: u8 = 0x04;
    pub const COMMAND_DISALLOWED: u8 = 0x0C;
    pub const INVALID_PARAMETERS: u8 = 0x12;
}

/// Command packets the controller accepts at once (every event says so).
const CREDITS: u8 = 1;

fn event(code: u8, params: &[u8]) -> Vec<u8> {
    let mut e = Vec::with_capacity(2 + params.len());
    e.push(code);
    e.push(params.len() as u8);
    e.extend_from_slice(params);
    e
}

/// Command Complete for `opcode`: `ret` is its return parameters, status first.
pub fn command_complete(opcode: u16, ret: &[u8]) -> Vec<u8> {
    let mut p = vec![CREDITS];
    p.extend_from_slice(&opcode.to_le_bytes());
    p.extend_from_slice(ret);
    event(ev::COMMAND_COMPLETE, &p)
}

/// Command Status for `opcode`.
pub fn command_status(opcode: u16, status: u8) -> Vec<u8> {
    let mut p = vec![status, CREDITS];
    p.extend_from_slice(&opcode.to_le_bytes());
    event(ev::COMMAND_STATUS, &p)
}

/// Any other event (the controller checks the host's event masks first).
pub fn other(code: u8, params: &[u8]) -> Vec<u8> {
    event(code, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_code_length_then_parameters() {
        // The exact bytes the AOSP HAL waits for after its own reset.
        assert_eq!(command_complete(op::RESET, &[status::SUCCESS]), [0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00]);
        assert_eq!(command_status(op::INQUIRY, status::SUCCESS), [0x0f, 0x04, 0x00, 0x01, 0x01, 0x04]);
        assert_eq!(other(ev::INQUIRY_COMPLETE, &[0]), [0x01, 0x01, 0x00]);
        assert!(op::is_vendor(0xFD53));
        assert!(!op::is_vendor(op::LE_RAND));
    }
}
