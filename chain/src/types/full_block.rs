// Copright 2019 chainnet.tech

use crate::types::{BlockHeader, Message, SignedMessage};

pub struct FullBlock {
    header: BlockHeader,
    bls_messages: Vec<Message>,
    secpk_messages: Vec<SignedMessage>,
}