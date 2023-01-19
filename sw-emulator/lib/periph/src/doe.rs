/*++

Licensed under the Apache-2.0 license.

File Name:

    doe.rs

Abstract:

    File contains Deobfuscation Engine Implementation

--*/

use crate::{KeyVault, SocRegisters};
use caliptra_emu_bus::{BusError, Clock, ReadWriteMemory, ReadWriteRegister, Timer, TimerAction};
use caliptra_emu_crypto::Aes256Cbc;
use caliptra_emu_derive::Bus;
use caliptra_emu_types::{RvData, RvSize};
use tock_registers::interfaces::{ReadWriteable, Readable, Writeable};
use tock_registers::register_bitfields;

/// Initialization vector size
const DOE_IV_SIZE: usize = 16;

/// The number of CPU clock cycles it takes to perform the hash update action.
const DOE_OP_TICKS: u64 = 1000;

register_bitfields! [
    u32,

    /// Control Register Fields
    Control [
        CMD OFFSET(0) NUMBITS(2) [
            IDLE = 0b00,
            DEOBFUSCATE_UDS = 0b01,
            DEOBFUSCATE_FE = 0b10,
            CLEAR_SECRETS = 0b11,
        ],
        DEST OFFSET(2) NUMBITS(3) [],
        FLOW_DONE OFFSET(5) NUMBITS(1) [],
    ],
];

#[derive(Bus)]
#[poll_fn(poll)]
pub struct Doe {
    /// Initialization Vector
    #[peripheral(offset = 0x0000_0000, mask = 0x0000_000f)]
    iv: ReadWriteMemory<DOE_IV_SIZE>,

    /// Control Register
    #[register(offset = 0x0000_0010, write_fn = on_write_control)]
    control: ReadWriteRegister<u32, Control::Register>,

    /// Timer
    timer: Timer,

    /// Key Vault
    key_vault: KeyVault,

    /// SOC Registers
    soc_reg: SocRegisters,

    /// Operation Complete Action
    op_complete_action: Option<TimerAction>,
}

impl Doe {
    /// Create new instance of deobfuscation engine
    ///
    /// # Arguments
    ///
    /// * `clock` - Clock
    /// * `key_vault` - Key Vault
    /// * `soc-rec` - SOC Registers
    ///
    /// # Returns
    ///
    /// * `Self` - Instance of deobfuscation engine
    pub fn new(clock: &Clock, key_vault: KeyVault, soc_reg: SocRegisters) -> Self {
        Self {
            iv: ReadWriteMemory::new(),
            control: ReadWriteRegister::new(0),
            timer: Timer::new(clock),
            key_vault,
            soc_reg,
            op_complete_action: None,
        }
    }

    /// On Write callback for `control` register
    ///
    /// # Arguments
    ///
    /// * `size` - Size of the write
    /// * `val` - Data to write
    ///
    /// # Error
    ///
    /// * `BusError` - Exception with cause `BusError::StoreAccessFault` or `BusError::StoreAddrMisaligned`
    pub fn on_write_control(&mut self, size: RvSize, val: RvData) -> Result<(), BusError> {
        // Writes have to be Word aligned
        if size != RvSize::Word {
            Err(BusError::StoreAccessFault)?
        }

        // Set the control register
        self.control.reg.set(val);

        if self.control.reg.read(Control::CMD) != Control::CMD::IDLE.value {
            self.control.reg.modify(Control::FLOW_DONE::CLEAR);
            self.op_complete_action = Some(self.timer.schedule_poll_in(DOE_OP_TICKS));
        }

        Ok(())
    }

    /// Called by Bus::poll() to indicate that time has passed
    fn poll(&mut self) {
        if self.timer.fired(&mut self.op_complete_action) {
            let key_id = self.control.reg.read(Control::DEST);
            match self.control.reg.read_as_enum(Control::CMD) {
                Some(Control::CMD::Value::DEOBFUSCATE_UDS) => self.unscramble_uds(key_id),
                Some(Control::CMD::Value::DEOBFUSCATE_FE) => self.unscramble_fe(key_id),
                Some(Control::CMD::Value::CLEAR_SECRETS) => self.clear_secrets(),
                _ => {}
            }
            self.control.reg.write(Control::FLOW_DONE::SET);
        }
    }

    /// Unscramble unique device secret  (UDS) and store it in key vault
    ///
    /// # Argument
    ///
    /// * `key_id` - Key index to store the UDS
    fn unscramble_uds(&mut self, key_id: u32) {
        let cipher_uds = self.soc_reg.uds();
        let mut plain_uds = [0u8; 64];
        Aes256Cbc::decrypt(
            &self.soc_reg.doe_key(),
            self.iv.data(),
            &cipher_uds,
            &mut plain_uds[..cipher_uds.len()],
        );
        self.key_vault.write_key(key_id, &plain_uds)
    }

    /// Unscramble field entropy and store it in key vault
    ///
    /// # Argument
    ///
    /// * `key_id` - Key index to store the field entropy
    fn unscramble_fe(&mut self, key_id: u32) {
        let cipher_fe = self.soc_reg.field_entropy();
        let mut plain_fe = [0u8; 64];
        Aes256Cbc::decrypt(
            &self.soc_reg.doe_key(),
            self.iv.data(),
            &cipher_fe[..64],
            &mut plain_fe,
        );
        self.key_vault.write_key(key_id, &plain_fe);
    }

    /// Clear secrets
    fn clear_secrets(&mut self) {
        self.soc_reg.clear_secrets()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caliptra_emu_bus::Bus;
    use caliptra_emu_types::RvAddr;
    use tock_registers::registers::InMemoryRegister;

    const OFFSET_IV: RvAddr = 0;
    const OFFSET_CONTROL: RvAddr = 0x10;

    fn make_word(idx: usize, arr: &[u8]) -> RvData {
        let mut res: RvData = 0;
        for i in 0..4 {
            res = res | ((arr[idx + i] as RvData) << i * 8);
        }
        res
    }

    #[test]
    fn test_deobfuscate_uds() {
        const IV: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];

        const PLAIN_TEXT_UDS: [u8; 48] = [
            0x6B, 0xC1, 0xBE, 0xE2, 0x2E, 0x40, 0x9F, 0x96, 0xE9, 0x3D, 0x7E, 0x11, 0x73, 0x93,
            0x17, 0x2A, 0xAE, 0x2D, 0x8A, 0x57, 0x1E, 0x3, 0xAC, 0x9C, 0x9E, 0xB7, 0x6F, 0xAC,
            0x45, 0xAF, 0x8E, 0x51, 0x30, 0xC8, 0x1C, 0x46, 0xA3, 0x5C, 0xE4, 0x11, 0xE5, 0xFB,
            0xC1, 0x19, 0x1A, 0xA, 0x52, 0xEF,
        ];

        let clock = Clock::new();
        let key_vault = KeyVault::new();
        let soc_reg = SocRegisters::new();
        let mut doe = Doe::new(&clock, key_vault.clone(), soc_reg.clone());

        for i in (0..IV.len()).step_by(4) {
            assert_eq!(
                doe.write(RvSize::Word, OFFSET_IV + i as RvAddr, make_word(i, &IV))
                    .ok(),
                Some(())
            );
        }

        assert_eq!(
            doe.write(
                RvSize::Word,
                OFFSET_CONTROL,
                (Control::CMD::DEOBFUSCATE_UDS + Control::DEST.val(2)).value
            )
            .ok(),
            Some(())
        );

        loop {
            let status = InMemoryRegister::<u32, Control::Register>::new(
                doe.read(RvSize::Word, OFFSET_CONTROL).unwrap(),
            );

            if status.is_set(Control::FLOW_DONE) {
                break;
            }

            clock.increment_and_poll(1, &mut doe);
        }

        assert_eq!(key_vault.read_key(2)[..48], PLAIN_TEXT_UDS);
    }

    #[test]
    fn test_deobfuscate_fe() {
        const PLAIN_TEXT_FE: [u8; 64] = [
            0xC6, 0x10, 0x65, 0x4D, 0xB4, 0xED, 0xA8, 0x53, 0xCF, 0x54, 0x6D, 0xEF, 0x52, 0x4E,
            0xC1, 0x5F, 0x39, 0xEF, 0x9A, 0xB2, 0x4B, 0x12, 0x57, 0xAC, 0x30, 0xAB, 0x92, 0x10,
            0xAD, 0xB1, 0x3E, 0xA0, 0x39, 0xEF, 0x9A, 0xB2, 0x4B, 0x12, 0x57, 0xAC, 0x30, 0xAB,
            0x92, 0x10, 0xAD, 0xB1, 0x3E, 0xA0, 0x39, 0xEF, 0x9A, 0xB2, 0x4B, 0x12, 0x57, 0xAC,
            0x30, 0xAB, 0x92, 0x10, 0xAD, 0xB1, 0x3E, 0xA0,
        ];

        let clock = Clock::new();
        let key_vault = KeyVault::new();
        let soc_reg = SocRegisters::new();
        let mut doe = Doe::new(&clock, key_vault.clone(), soc_reg.clone());

        let iv = [0u8; DOE_IV_SIZE];
        for i in (0..iv.len()).step_by(4) {
            assert_eq!(
                doe.write(RvSize::Word, OFFSET_IV + i as RvAddr, make_word(i, &iv))
                    .ok(),
                Some(())
            );
        }

        assert_eq!(
            doe.write(
                RvSize::Word,
                OFFSET_CONTROL,
                (Control::CMD::DEOBFUSCATE_FE + Control::DEST.val(3)).value
            )
            .ok(),
            Some(())
        );

        loop {
            let status = InMemoryRegister::<u32, Control::Register>::new(
                doe.read(RvSize::Word, OFFSET_CONTROL).unwrap(),
            );

            if status.is_set(Control::FLOW_DONE) {
                break;
            }

            clock.increment_and_poll(1, &mut doe);
        }

        assert_eq!(key_vault.read_key(3), PLAIN_TEXT_FE);
    }

    #[test]
    fn test_clear_secrets() {
        let expected_uds = [0u8; 48];
        let expected_doe_key = [0u8; 32];
        let expected_fe = [0u8; 128];
        let clock = Clock::new();
        let key_vault = KeyVault::new();
        let soc_reg = SocRegisters::new();
        let mut doe = Doe::new(&clock, key_vault.clone(), soc_reg.clone());
        assert_ne!(soc_reg.uds(), expected_uds);
        assert_ne!(soc_reg.doe_key(), expected_doe_key);
        assert_ne!(soc_reg.field_entropy(), expected_fe);

        assert_eq!(
            doe.write(
                RvSize::Word,
                OFFSET_CONTROL,
                (Control::CMD::CLEAR_SECRETS + Control::DEST.val(0)).value
            )
            .ok(),
            Some(())
        );

        loop {
            let status = InMemoryRegister::<u32, Control::Register>::new(
                doe.read(RvSize::Word, OFFSET_CONTROL).unwrap(),
            );

            if status.is_set(Control::FLOW_DONE) {
                break;
            }

            clock.increment_and_poll(1, &mut doe);
        }

        assert_eq!(soc_reg.uds(), expected_uds);
        assert_eq!(soc_reg.doe_key(), expected_doe_key);
        assert_eq!(soc_reg.field_entropy(), expected_fe);
    }
}