use jubjub::Fr;
use rusqlite::Connection;
use std::{collections::HashMap, mem::swap, sync::mpsc::channel};

use crate::{
    db::{
        account::{get_account_info, list_accounts},
        notes::list_received_notes,
    },
    lwd::rpc::{Bridge, CompactBlock},
    types::AccountInfo,
    warp::try_sapling_decrypt,
    Hash,
};
use anyhow::Result;
use rayon::prelude::*;
use tracing::info;
use zcash_primitives::{
    consensus::Network,
    sapling::{value::NoteValue, Note, PaymentAddress, Rseed},
};

use crate::warp::{hasher::SaplingHasher, Edge, Hasher, MERKLE_DEPTH};

use super::{ReceivedNote, TxValueUpdate};

#[derive(Debug)]
pub struct Synchronizer {
    pub hasher: SaplingHasher,
    pub network: Network,
    pub account_infos: Vec<AccountInfo>,
    pub start: u32,
    pub notes: Vec<ReceivedNote>,
    pub spends: Vec<TxValueUpdate<Hash>>,
    pub position: u32,
    pub tree_state: Edge,
}

struct BridgeExt<'a> {
    b: &'a Bridge,
    s: i32,
    e: i32,
}

impl Synchronizer {
    pub fn new(
        network: &Network,
        connection: &Connection,
        start: u32,
        position: u32,
        tree_state: Edge,
    ) -> Result<Self> {
        let accounts = list_accounts(connection)?;
        let mut account_infos = vec![];
        for a in accounts {
            let ai = get_account_info(network, connection, a.account)?;
            account_infos.push(ai);
        }
        let notes = list_received_notes(connection, start, false)?;

        Ok(Self {
            hasher: SaplingHasher::default(),
            network: *network,
            account_infos,
            start,
            notes,
            spends: vec![],
            position,
            tree_state,
        })
    }

    pub fn add(&mut self, blocks: &[CompactBlock]) -> Result<()> {
        let ivks = self
            .account_infos
            .iter()
            .map(|ai| (ai.account, ai.sapling.vk.fvk.vk.ivk()))
            .collect::<Vec<_>>();

        let outputs = blocks.into_par_iter().flat_map_iter(|b| {
            b.vtx.iter().enumerate().flat_map(move |(ivtx, vtx)| {
                vtx.outputs
                    .iter()
                    .enumerate()
                    .map(move |(vout, o)| (b.height, b.time, ivtx, vout, o))
            })
        });

        let (sender, receiver) = channel();
        outputs
            .into_par_iter()
            .for_each_with(sender, |sender, (height, time, ivtx, vout, o)| {
                try_sapling_decrypt(
                    &self.network,
                    &ivks,
                    height as u32,
                    time,
                    ivtx as u32,
                    vout as u32,
                    o,
                    sender,
                )
                .unwrap();
            });

        let mut notes = vec![];
        while let Ok(mut note) = receiver.recv() {
            let mut position = self.position;
            for cb in blocks.iter() {
                if cb.height as u32 == note.height {
                    break;
                }
                for tx in cb.vtx.iter() {
                    position += tx.outputs.len() as u32;
                    position += tx
                        .sapling_bridge
                        .as_ref()
                        .map(|b| b.len as u32)
                        .unwrap_or_default();
                }
            }
            let cb = &blocks[(note.height - self.start - 1) as usize];
            for (ivtx, tx) in cb.vtx.iter().enumerate() {
                if ivtx as u32 == note.tx.ivtx {
                    break;
                }
                position += tx.outputs.len() as u32;
                position += tx
                    .sapling_bridge
                    .as_ref()
                    .map(|b| b.len as u32)
                    .unwrap_or_default();
            }
            position += note.vout;
            note.position = position;

            let ai = self
                .account_infos
                .iter()
                .find(|&ai| ai.account == note.account)
                .unwrap();
            let recipient = PaymentAddress::from_bytes(&note.address).unwrap();
            let vk = &ai.sapling.vk.fvk.vk;
            let n = Note::from_parts(
                recipient,
                NoteValue::from_raw(note.value),
                Rseed::BeforeZip212(Fr::from_bytes(&note.rcm).unwrap()),
            );
            let nf = n.nf(&vk.nk, note.position as u64);
            note.nf = nf.0;
            note.tx.txid = cb.vtx[note.tx.ivtx as usize]
                .hash
                .clone()
                .try_into()
                .unwrap();
            notes.push(note);
        }

        let mut bridges = vec![];
        let mut p = self.position;
        for cb in blocks.iter() {
            for tx in cb.vtx.iter() {
                p += tx.outputs.len() as u32;
                if let Some(b) = &tx.sapling_bridge {
                    let be = BridgeExt {
                        b,
                        s: p as i32,
                        e: (p + b.len - 1) as i32,
                    };
                    bridges.push(be);
                    p += b.len;
                }
            }
        }

        let mut cmxs = vec![];
        let mut count_cmxs = 0;

        for depth in 0..MERKLE_DEPTH {
            let mut position = self.position >> depth;
            if position % 2 == 1 {
                cmxs.insert(0, Some(self.tree_state.0[depth].unwrap()));
                position -= 1;
            }

            if depth == 0 {
                for cb in blocks.iter() {
                    for vtx in cb.vtx.iter() {
                        for co in vtx.outputs.iter() {
                            cmxs.push(Some(co.cmu.clone().try_into().unwrap()));
                        }
                        count_cmxs += vtx.outputs.len();
                        if let Some(b) = &vtx.sapling_bridge {
                            for _ in 0..b.len {
                                cmxs.push(None);
                            }
                            count_cmxs += b.len as usize;
                        }
                    }
                }
            }

            for n in notes.iter_mut() {
                let npos = n.position >> depth;
                let nidx = (npos - position) as usize;

                if depth == 0 {
                    n.witness.position = npos;
                    n.witness.value = cmxs[nidx].unwrap();
                }

                if nidx % 2 == 0 {
                    if nidx + 1 < cmxs.len() {
                        n.witness.ommers.0[depth] = cmxs[nidx + 1];
                    } else {
                        n.witness.ommers.0[depth] = None;
                    }
                } else {
                    n.witness.ommers.0[depth] = cmxs[nidx - 1];
                }
            }

            for be in bridges.iter_mut() {
                let b = be.b;
                if depth >= b.levels.len() {
                    continue;
                }
                let level = &b.levels[depth];

                let s = ((be.s as u32 - position) & 0xFFFE) as usize; // round to pair
                if let Some(side) = &level.head {
                    let side = side.side.as_ref().unwrap();
                    match side {
                        crate::lwd::rpc::either::Side::Left(h) => {
                            cmxs[s] = Some(h.clone().try_into().unwrap());
                        },
                        crate::lwd::rpc::either::Side::Right(h) => {
                            cmxs[s + 1] = Some(h.clone().try_into().unwrap());
                        }
                    }
                }

                let e = ((be.e as u32 - position) & 0xFFFE) as usize; // round to pair
                if let Some(side) = &level.tail {
                    let side = side.side.as_ref().unwrap();
                    match side {
                        crate::lwd::rpc::either::Side::Left(h) => {
                            cmxs[e] = Some(h.clone().try_into().unwrap());
                        },
                        crate::lwd::rpc::either::Side::Right(h) => {
                            cmxs[e + 1] = Some(h.clone().try_into().unwrap());
                        }
                    }

                }

                be.s = be.s / 2;
                be.e = (be.e - 1) / 2;
            }

            let len = cmxs.len();
            if len >= 2 {
                for n in self.notes.iter_mut() {
                    if n.witness.ommers.0[depth].is_none() {
                        n.witness.ommers.0[depth] = cmxs[1];
                    }
                }
            }

            if len % 2 == 1 {
                self.tree_state.0[depth] = cmxs[len - 1];
            } else {
                self.tree_state.0[depth] = None;
            }

            let pairs = len / 2;
            let mut cmxs2 = self.hasher.parallel_combine_opt(depth as u8, &cmxs, pairs);
            swap(&mut cmxs, &mut cmxs2);
        }

        tracing::info!("Old notes #{}", self.notes.len());
        tracing::info!("New notes #{}", notes.len());
        self.notes.append(&mut notes);
        self.position += count_cmxs as u32;
        self.start += blocks.len() as u32;

        // detect spends

        let mut nfs = self
            .notes
            .iter_mut()
            .map(|n| (n.nf.clone(), n))
            .collect::<HashMap<_, _>>();
        for cb in blocks.iter() {
            for vtx in cb.vtx.iter() {
                for sp in vtx.spends.iter() {
                    let nf = &*sp.nf;
                    if let Some(n) = nfs.get_mut(nf) {
                        n.spent = Some(cb.height as u32);
                        let tx = TxValueUpdate::<Hash> {
                            account: n.account,
                            txid: vtx.hash.clone().try_into().unwrap(),
                            value: -(n.value as i64),
                            id_tx: 0,
                            height: cb.height as u32,
                            id_spent: Some(n.nf),
                        };
                        self.spends.push(tx);
                    }
                }
            }
        }

        info!("# {}", self.notes.len());
        let auth_path = self.tree_state.to_auth_path(&self.hasher);
        for note in self.notes.iter() {
            let root = note.witness.root(&auth_path, &self.hasher);
            info!("{}", hex::encode(&root));
        }

        Ok(())
    }
}
