//! The upgrade status message is a BNB extension to the eth standard.
use reth_codecs::derive_arbitrary;
use reth_rlp::{RlpDecodable, RlpEncodable};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// An UpgradeStatus message which is used by BSC
#[derive_arbitrary(rlp)]
#[derive(Copy, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct UpgradeStatus {
    /// bsc overload: to disable tx broadcast
    pub extensions: UpgradeStatusExtensions,
}

/// The upgrade status message is a BNB extension to the eth standard.
#[derive_arbitrary(rlp)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, RlpEncodable, RlpDecodable)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct UpgradeStatusExtensions {
    /// bsc overload: disabling peer broadcast flag
    pub disabled_peer_tx_broadcast: bool,
}

impl Default for UpgradeStatusExtensions {
    fn default() -> Self {
        Self {
            disabled_peer_tx_broadcast: false,
        }
    }
}
