use anyhow::Result;
use rusqlite::Connection;
use zcash_client_backend::encoding::AddressCodec;
use zcash_primitives::{consensus::Network, legacy::TransparentAddress};

use crate::{
    db::{
        account::{get_account_info, list_accounts},
        notes::list_utxos,
    }, types::CheckpointHeight, warp::{OutPoint, TransparentTx, UTXO}
};

use super::{ReceivedTx, TxValueUpdate};

pub struct TransparentSync {
    pub network: Network,
    pub addresses: Vec<(u32, TransparentAddress)>,
    pub utxos: Vec<UTXO>,
    pub txs: Vec<(ReceivedTx, OutPoint, u64)>,
    pub tx_updates: Vec<TxValueUpdate<OutPoint>>,
}

impl TransparentSync {
    pub fn new(network: &Network, connection: &Connection, height: CheckpointHeight) -> Result<Self> {
        let accounts = list_accounts(connection)?;
        let mut addresses = vec![];
        for a in accounts.iter() {
            let ai = get_account_info(network, connection, a.id)?;
            let taddr = ai.transparent.as_ref().map(|ti| ti.addr);
            if let Some(taddr) = taddr {
                addresses.push((a.id, taddr));
            }
        }
        let utxos = list_utxos(connection, height)?;

        Ok(Self {
            network: network.clone(),
            addresses,
            utxos,
            txs: vec![],
            tx_updates: vec![],
        })
    }

    pub fn process_txs(&mut self, txs: &[TransparentTx]) -> Result<()> {
        for tx in txs {
            for vin in tx.vins.iter() {
                let r = self
                    .utxos
                    .iter()
                    .find(|&utxo| utxo.txid == vin.txid && utxo.vout == vin.vout);
                if let Some(utxo) = r {
                    let tx_value = TxValueUpdate::<OutPoint> {
                        id_tx: 0,
                        account: tx.account,
                        txid: tx.txid,
                        value: -(utxo.value as i64),
                        height: tx.height,
                        id_spent: Some(OutPoint {
                            txid: vin.txid,
                            vout: vin.vout,
                        }),
                    };
                    self.tx_updates.push(tx_value);
                }
            }
            for txout in tx.vouts.iter() {
                let rtx = ReceivedTx {
                    id: 0,
                    account: tx.account,
                    height: tx.height,
                    txid: tx.txid,
                    timestamp: tx.timestamp,
                    ivtx: 0,
                    value: 0,
                };
                self.txs.push((
                    rtx,
                    OutPoint {
                        txid: tx.txid,
                        vout: txout.vout,
                    },
                    txout.value,
                ));
                // outputs are filtered for our account
                let (_, ta) = self
                    .addresses
                    .iter()
                    .find(|(account, _)| *account == tx.account)
                    .unwrap();
                let address = ta.encode(&self.network);
                self.utxos.push(UTXO {
                    is_new: true,
                    id: 0,
                    account: tx.account,
                    height: tx.height,
                    txid: tx.txid,
                    vout: txout.vout,
                    address,
                    value: txout.value,
                });
            }
        }
        // detect our spends in vins
        // mark utxo as spent
        // detect incoming funds in vouts
        // add new utxo
        // update txs table with net value

        Ok(())
    }
}
