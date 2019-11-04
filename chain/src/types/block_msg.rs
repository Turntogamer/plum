// Copyright 2019 chainnet.tech

use crate::types::{BlockHeader, Cid};

pub struct BlockMsg {
    header: BlockHeader,
    bls_messages: Vec<Cid>,
    secpk_messages: Vec<Cid>,
}