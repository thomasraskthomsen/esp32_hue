use trouble_host::prelude::{
    Address, BdAddr, BondInformation, IdentityResolvingKey, LongTermKey, SecurityLevel, AddrKind,
};
use trouble_host::Identity;

use crate::{HueController, Stack};

/// What's kept of a pairing with a light, to encrypt the link again without
/// pairing anew.
#[derive(Clone, Debug, PartialEq)]
pub struct Bond {
    /// The light's identity address, least significant byte first.
    pub address: [u8; 6],
    /// Whether the address is a random static one rather than a public one.
    pub random_address: bool,
    /// Long term key.
    pub ltk: u128,
    /// Identity resolving key, if the light shared one.
    pub irk: Option<u128>,
    /// Whether the key was made with MITM protection.
    pub authenticated: bool,
}

impl Bond {
    /// The light's address.
    pub fn light_address(&self) -> Address {
        self.to_information().identity.addr
    }

    pub(crate) fn to_information(&self) -> BondInformation {
        let level = if self.authenticated {
            SecurityLevel::EncryptedAuthenticated
        } else {
            SecurityLevel::Encrypted
        };
        let kind = if self.random_address { AddrKind::RANDOM } else { AddrKind::PUBLIC };
        BondInformation {
            ltk: LongTermKey(self.ltk),
            identity: Identity {
                addr: Address {
                    kind,
                    addr: BdAddr::new(self.address),
                },
                irk: self
                    .irk
                    .and_then(core::num::NonZeroU128::new)
                    .map(IdentityResolvingKey),
            },
            is_bonded: true,
            security_level: level,
        }
    }

    pub(crate) fn from_information(bond: &BondInformation) -> Self {
        Self {
            address: bond.identity.addr.addr.raw().try_into().unwrap_or_default(),
            random_address: bond.identity.addr.kind == AddrKind::RANDOM,
            ltk: bond.ltk.0,
            irk: bond.identity.irk.map(|irk| irk.0.get()),
            authenticated: bond.security_level == SecurityLevel::EncryptedAuthenticated,
        }
    }
}

/// Tells the stack about stored bonds, so it can encrypt links to those
/// lights. Call it at startup.
pub fn add_bonds<C: HueController>(stack: &Stack<'_, C>, bonds: &[Bond]) {
    for bond in bonds {
        if let Err(err) = stack.add_bond_information(bond.to_information()) {
            log::warn!("BLE: could not use stored bond: {err:?}");
        }
    }
}

/// Makes the stack forget bonds (the lights still remember the stack).
pub fn remove_bonds<C: HueController>(stack: &Stack<'_, C>, bonds: &[Bond]) {
    for bond in bonds {
        let _ = stack.remove_bond_information(bond.to_information().identity);
    }
}
