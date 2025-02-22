use anyhow::Result;
use rpc::{
    BlockId, BlockRange, CompactBlock, Empty, RawTransaction, TransparentAddressBlockFilter,
    TreeState, TxFilter,
};
use tokio::runtime::Handle;
use tonic::{Request, Streaming};
use zcash_client_backend::encoding::AddressCodec as _;
use zcash_primitives::{
    consensus::{BlockHeight, BranchId, Network},
    legacy::TransparentAddress,
    transaction::Transaction,
};

use crate::{
    coin::connect_lwd, types::CheckpointHeight, warp::{legacy::CommitmentTreeFrontier, OutPoint, TransparentTx, TxOut2}, Client
};

#[path = "./generated/cash.z.wallet.sdk.rpc.rs"]
pub mod rpc;

pub async fn get_last_height(client: &mut Client) -> Result<u32> {
    let r = client
        .get_lightd_info(Request::new(Empty {}))
        .await?
        .into_inner();
    Ok(r.block_height as u32)
}

pub async fn get_tree_state(
    client: &mut Client,
    height: CheckpointHeight,
) -> Result<(CommitmentTreeFrontier, CommitmentTreeFrontier)> {
    let height: u32 = height.into();
    let tree_state = client
        .get_tree_state(Request::new(BlockId {
            height: height as u64,
            hash: vec![],
        }))
        .await?
        .into_inner();

    let TreeState {
        sapling_tree,
        orchard_tree,
        ..
    } = tree_state;

    fn decode_tree_state(s: &str) -> CommitmentTreeFrontier {
        if s.is_empty() {
            CommitmentTreeFrontier::default()
        } else {
            let tree = hex::decode(s).unwrap();
            CommitmentTreeFrontier::read(&*tree).unwrap()
        }
    }

    let sapling = decode_tree_state(&sapling_tree);
    let orchard = decode_tree_state(&orchard_tree);

    #[cfg(test)]
    {
        // use crate::warp::hasher::SaplingHasher;
        // use sapling_crypto::{CommitmentTree, Node};

        // let st = hex::decode(&sapling_tree).unwrap();
        // let st = CommitmentTree::read(&*st)?;
        // let root1 = st.root();
        // println!("{}", hex::encode(&root1.repr));
        // let s_hasher = SaplingHasher::default();
        // let edge = sapling.to_edge(&s_hasher);
        // let root2 = edge.root(&s_hasher);
        // println!("{}", hex::encode(&root2));
        // assert_eq!(root1.repr, root2);
    }

    Ok((sapling, orchard))
}

pub async fn get_compact_block(client: &mut Client, height: u32) -> Result<CompactBlock> {
    let mut blocks = client
        .get_block_range(Request::new(BlockRange {
            start: Some(BlockId {
                height: height as u64,
                hash: vec![],
            }),
            end: Some(BlockId {
                height: height as u64,
                hash: vec![],
            }),
            spam_filter_threshold: 0,
        }))
        .await?
        .into_inner();
    while let Some(block) = blocks.message().await? {
        return Ok(block);
    }
    Err(anyhow::anyhow!("No block found"))
}

pub async fn get_compact_block_range(
    client: &mut Client,
    start: u32,
    end: u32,
) -> Result<Streaming<CompactBlock>> {
    let req = || {
        Request::new(BlockRange {
            start: Some(BlockId {
                height: start as u64,
                hash: vec![],
            }),
            end: Some(BlockId {
                height: end as u64,
                hash: vec![],
            }),
            spam_filter_threshold: 0,
        })
    };
    let blocks = client.get_block_range(req()).await?.into_inner();
    Ok(blocks)
}

pub async fn get_transparent(
    network: &Network,
    client: &mut Client,
    account: u32,
    taddr: TransparentAddress,
    start: u32,
    end: u32,
) -> Result<Vec<TransparentTx>> {
    let mut txs = client
        .get_taddress_txids(Request::new(TransparentAddressBlockFilter {
            address: taddr.encode(network),
            range: Some(BlockRange {
                start: Some(BlockId {
                    height: start as u64,
                    hash: vec![],
                }),
                end: Some(BlockId {
                    height: end as u64,
                    hash: vec![],
                }),
                spam_filter_threshold: 0,
            }),
        }))
        .await?
        .into_inner();
    let mut ttxs = vec![];
    while let Some(raw_tx) = txs.message().await? {
        let height = raw_tx.height as u32;
        let raw_tx = raw_tx.data;
        let branch_id = BranchId::for_height(network, BlockHeight::from_u32(height));
        let tx = Transaction::read(&*raw_tx, branch_id)?;
        let transparent_bundle = tx.transparent_bundle().unwrap();
        let mut vins = vec![];
        for vin in transparent_bundle.vin.iter() {
            let prev_out = crate::warp::OutPoint {
                txid: vin.prevout.hash().clone(),
                vout: vin.prevout.n(),
            };
            vins.push(prev_out);
        }
        let mut vouts = vec![];
        for (vout, txout) in transparent_bundle.vout.iter().enumerate() {
            if let Some(address) = txout.recipient_address() {
                if address == taddr {
                    let out = crate::warp::TxOut {
                        address: txout.recipient_address(),
                        value: txout.value.into(),
                        vout: vout as u32,
                    };
                    vouts.push(out);
                }
            }
        }
        let ttx = TransparentTx {
            account,
            height,
            timestamp: 0, // TODO: Resolve timestamp from block header
            txid: tx.txid().as_ref().clone().try_into().unwrap(),
            vins,
            vouts,
        };
        ttxs.push(ttx);
    }

    Ok(ttxs)
}

pub async fn broadcast(client: &mut Client, height: u32, tx: &[u8]) -> Result<String> {
    let res = client
        .send_transaction(Request::new(RawTransaction {
            data: tx.to_vec(),
            height: height as u64,
        }))
        .await?
        .into_inner();
    Ok(res.error_message)
}

pub fn get_txin_coins(network: Network, url: String, ops: Vec<OutPoint>) -> Result<Vec<TxOut2>> {
    tokio::task::block_in_place(move || {
        Handle::current().block_on(async move {
            let mut client = connect_lwd(&url).await?;
            let mut txouts = vec![];
            for op in ops {
                let tx = client
                    .get_transaction(Request::new(TxFilter {
                        block: None,
                        index: 0,
                        hash: op.txid.to_vec(),
                    }))
                    .await?
                    .into_inner();
                let data = &*tx.data;
                let tx = Transaction::read(data, BranchId::Nu5)?;
                let tx_data = tx.into_data();
                let b = tx_data
                    .transparent_bundle()
                    .ok_or(anyhow::anyhow!("No T bundle"))?;
                let txout = &b.vout[op.vout as usize];
                let txout = TxOut2 {
                    address: txout.recipient_address().map(|o| o.encode(&network)),
                    value: txout.value.into(),
                    vout: op.vout,
                };
                txouts.push(txout);
            }
            Ok(txouts)
        })
    })
}

pub async fn get_transaction(
    network: &Network,
    client: &mut Client,
    txid: &[u8],
) -> Result<(u32, Transaction)> {
    let tx = client
        .get_transaction(Request::new(TxFilter {
            block: None,
            index: 0,
            hash: txid.to_vec(),
        }))
        .await?
        .into_inner();
    let height = tx.height as u32;
    let tx = Transaction::read(
        &*tx.data,
        BranchId::for_height(network, BlockHeight::from_u32(height)),
    )?;
    Ok((height, tx))
}
