use std::{
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use clap_repl::{
    reedline::{DefaultPrompt, DefaultPromptSegment, FileBackedHistory},
    ClapEditor,
};
use console::style;
use figment::{
    providers::{Env, Format as _, Toml},
    Figment,
};
use flatbuffers::{FlatBufferBuilder, WIPOffset};
use parking_lot::Mutex;
use rand::rngs::OsRng;
use rusqlite::{Connection, DropBehavior};
use serde::Deserialize;
use zcash_keys::address::Address as RecipientAddress;
use zcash_protocol::consensus::{Network, NetworkUpgrade, Parameters};

use crate::{
    account::{
        address::get_diversified_address,
        contacts::{add_contact, commit_unsaved_contacts},
        txs::get_txs,
    },
    coin::CoinDef,
    data::fb::{PaymentRequestT, ShieldedNote, TransactionInfo},
    db::{
        account::{get_account_info, get_balance, list_accounts},
        account_manager::{
            create_new_account, delete_account, detect_key, edit_account_birth, edit_account_name,
            get_min_birth,
        },
        contacts::{delete_contact, edit_contact_address, edit_contact_name, list_contacts},
        notes::{
            get_sync_height, get_txid, get_unspent_notes, snap_to_checkpoint, store_block, store_tx_details, truncate_scan
        },
        reset_tables,
        tx::{get_tx_details, list_messages},
    },
    fb_vec_to_bytes,
    keys::{generate_random_mnemonic_phrase, TSKStore},
    lwd::{broadcast, get_compact_block, get_last_height, get_transaction, get_tree_state},
    pay::{
        make_payment,
        sweep::{prepare_sweep, scan_utxo_by_seed},
        Payment, PaymentItem, UnsignedTransaction,
    },
    txdetails::{analyze_raw_transaction, decode_tx_details, retrieve_tx_details},
    types::{CheckpointHeight, PoolMask},
    utils::{
        db::encrypt_db,
        ua::decode_ua,
        uri::{make_payment_uri, parse_payment_uri},
    },
    warp::{sync::warp_sync, BlockHeader},
    EXPIRATION_HEIGHT_DELTA,
};

#[derive(Deserialize)]
pub struct Config {
    pub db_path: String,
    pub lwd_url: String,
    pub warp_url: String,
    pub warp_end_height: u32,
    pub seed: String,
    pub confirmations: u32,
}

#[derive(Parser, Clone, Debug)]
pub struct Account {
    #[structopt(subcommand)]
    command: AccountCommand,
}

#[derive(Subcommand, Clone, Debug)]
pub enum AccountCommand {
    List,
    Create {
        key: Option<String>,
        name: Option<String>,
        birth: Option<u32>,
    },
    EditName {
        account: u32,
        name: String,
    },
    EditBirthHeight {
        account: u32,
        birth: u32,
    },
    Delete {
        account: u32,
    },
}

#[derive(Parser, Clone, Debug)]
pub struct Contact {
    #[structopt(subcommand)]
    command: ContactCommand,
}

#[derive(Subcommand, Clone, Debug)]
pub enum ContactCommand {
    List,
    Create {
        account: u32,
        name: String,
        address: String,
    },
    EditName {
        id: u32,
        name: String,
    },
    EditAddress {
        id: u32,
        address: String,
    },
    Delete {
        id: u32,
    },
    Save {
        account: u32,
    },
}

/// The enum of sub-commands supported by the CLI
#[derive(Parser, Clone, Debug)]
pub enum Command {
    Account(Account),
    Contact(Contact),
    CreateDatabase,
    GenerateSeed,
    Backup {
        account: u32,
    },
    EncryptDb {
        password: String,
        new_db_path: String,
    },
    SetDbPassword {
        password: String,
    },
    LastHeight,
    SyncHeight,
    Reset {
        height: Option<u32>,
    },
    Sync {
        confirmations: Option<u32>,
    },
    Address {
        account: u32,
        mask: u8,
    },
    GetTx {
        account: u32,
        id: u32,
    },
    Balance {
        account: u32,
    },
    GenDiversifiedAddress {
        account: u32,
        pools: u8,
    },
    Pay {
        account: u32,
        address: String,
        amount: u64,
        pools: u8,
        fee_paid_by_sender: u8,
    },
    Sweep {
        account: u32,
        destination_address: String,
    },
    GetTxDetails {
        id: u32,
    },
    DecodeAddress {
        address: String,
    },
    ListTxs {
        account: u32,
    },
    ListNotes {
        account: u32,
    },
    ListMessages {
        account: u32,
    },
    DecodeUA {
        ua: String,
    },
    MakePaymentURI {
        recipients: Vec<PaymentRequestT>,
    },
    PayPaymentUri {
        account: u32,
        uri: String,
    },
    BroadcastLatest {
        clear: Option<u8>,
    },
}

impl FromStr for PaymentRequestT {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        serde_json::from_str::<PaymentRequestT>(s)
    }
}

fn display_tx(
    network: &Network,
    connection: &Connection,
    cp_height: CheckpointHeight,
    unsigned_tx: UnsignedTransaction,
    tsk_store: &mut TSKStore,
) -> Result<Vec<u8>> {
    let mut summary = unsigned_tx.to_summary()?;
    summary.detach();
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    let txb = unsigned_tx.build(
        network,
        &connection,
        cp_height.0 + EXPIRATION_HEIGHT_DELTA,
        tsk_store,
        OsRng,
    )?;
    Ok(txb)
}

#[tokio::main]
async fn process_command(command: Command, zec: &mut CoinDef, txbytes: &mut Vec<u8>) -> Result<()> {
    let network = &zec.network;
    match command {
        Command::CreateDatabase => {
            let connection = zec.connection().unwrap();
            reset_tables(&connection)?;
        }
        Command::EncryptDb {
            password,
            new_db_path,
        } => {
            let connection = zec.connection()?;
            encrypt_db(&connection, &password, &new_db_path)?;
        }
        Command::SetDbPassword { password } => {
            zec.db_password = Some(password);
        }
        Command::Account(account_cmd) => {
            let connection = zec.connection()?;
            match account_cmd.command {
                AccountCommand::List => {
                    let accounts = list_accounts(&connection)?;
                    println!("{}", serde_json::to_string_pretty(&accounts)?);
                }
                AccountCommand::Create { key, name, birth } => {
                    let mut client = zec.connect_lwd().await?;
                    let bc_height = get_last_height(&mut client).await?;
                    let key = key.unwrap_or(CONFIG.seed.clone());
                    let name = name.unwrap_or("<unnamed>".to_string());
                    let kt = detect_key(network, &key, 0, 0)?;
                    let birth = birth.unwrap_or(bc_height);
                    create_new_account(network, &connection, &name, kt, birth)?;
                }
                AccountCommand::EditName { account, name } => {
                    edit_account_name(&connection, account, &name)?;
                }
                AccountCommand::EditBirthHeight { account, birth } => {
                    edit_account_birth(&connection, account, birth)?;
                }
                AccountCommand::Delete { account } => {
                    delete_account(&connection, account)?;
                }
            }
        }
        Command::Contact(contact_cmd) => {
            let connection = zec.connection()?;
            match contact_cmd.command {
                ContactCommand::List => {
                    let contacts = list_contacts(network, &connection)?;
                    let cards = contacts.iter().map(|c| c.card.clone()).collect::<Vec<_>>();
                    println!("{}", serde_json::to_string_pretty(&cards).unwrap());
                }
                ContactCommand::Create {
                    account,
                    name,
                    address,
                } => {
                    add_contact(&connection, account, &name, &address, false)?;
                }
                ContactCommand::EditName { id, name } => {
                    edit_contact_name(&connection, id, &name)?;
                }
                ContactCommand::EditAddress { id, address } => {
                    edit_contact_address(&connection, id, &address)?;
                }
                ContactCommand::Delete { id } => {
                    delete_contact(&connection, id)?;
                }
                ContactCommand::Save { account } => {
                    let mut client = zec.connect_lwd().await?;
                    let bc_height = get_last_height(&mut client).await?;
                    let cp_height = snap_to_checkpoint(&connection, bc_height - CONFIG.confirmations + 1)?;
                    let (s_tree, o_tree) = get_tree_state(&mut client, cp_height).await?;
                    let unsigned_tx = commit_unsaved_contacts(
                        network,
                        &connection,
                        account,
                        7,
                        cp_height,
                        &s_tree,
                        &o_tree,
                    )?;
                    *txbytes = display_tx(network, &connection, cp_height, unsigned_tx, &mut TSKStore::default())?;
                }
            }
        }
        Command::GenerateSeed => {
            let seed = generate_random_mnemonic_phrase(&mut OsRng);
            println!("{seed}");
        }
        Command::Backup { account } => {
            let connection = zec.connection()?;
            let ai = get_account_info(network, &connection, account)?;
            let backup = ai.to_backup(network);
            println!("{}", serde_json::to_string_pretty(&backup).unwrap());
        }
        Command::LastHeight => {
            let mut client = zec.connect_lwd().await?;
            let bc_height = get_last_height(&mut client).await?;
            println!("{bc_height}");
        }
        Command::SyncHeight => {
            let connection = zec.connection()?;
            let height = get_sync_height(&connection)?;
            println!("{height:?}");
        }
        Command::Reset { height } => {
            let connection = zec.connection()?;
            truncate_scan(&connection)?;
            let activation: u32 = network
                .activation_height(NetworkUpgrade::Sapling)
                .unwrap()
                .into();
            let min_birth_height = get_min_birth(&connection)?.unwrap_or(activation);
            let height = height.unwrap_or(min_birth_height).max(activation + 1);
            let mut client = zec.connect_lwd().await?;
            let block = get_compact_block(&mut client, height).await?;
            let mut connection = zec.connection()?;
            let mut transaction = connection.transaction()?;
            transaction.set_drop_behavior(DropBehavior::Commit);
            store_block(&transaction, &BlockHeader::from(&block))?;
        }
        Command::Sync { confirmations } => loop {
            let mut client = zec.connect_lwd().await?;
            let bc_height = get_last_height(&mut client).await?;
            let confirmations = confirmations.unwrap_or(1);
            if confirmations == 0 {
                anyhow::bail!("# Confirmations must be > 0");
            }
            let connection = zec.connection()?;
            let end_height = bc_height - confirmations + 1;
            let start_height = get_sync_height(&connection)?.ok_or(anyhow::anyhow!("no sync data. Have you run reset?"))?;
            if start_height >= end_height {
                break;
            }
            let end_height = (start_height + 100_000).min(end_height);
            warp_sync(&zec, CheckpointHeight(start_height), end_height).await?;
            let connection = Mutex::new(zec.connection()?);
            retrieve_tx_details(network, connection, zec.url.clone()).await?;
        },
        Command::Address { account, mask } => {
            let connection = zec.connection()?;
            let ai = get_account_info(network, &connection, account)?;
            let address = ai
                .to_address(network, PoolMask(mask))
                .ok_or(anyhow::anyhow!("Invalid mask"))?;
            println!("Address: {}", address);
        }
        Command::Balance { account } => {
            let connection = zec.connection()?;
            let height = get_sync_height(&connection)?.unwrap_or_default();
            let balance = get_balance(&connection, account, height)?;
            println!("Balance: {:?}", balance);
        }
        Command::Pay {
            account,
            address,
            amount,
            pools,
            fee_paid_by_sender,
        } => {
            let mut client = zec.connect_lwd().await?;
            let bc_height = get_last_height(&mut client).await?;
            let connection = zec.connection()?;
            let cp_height = snap_to_checkpoint(&connection, bc_height - CONFIG.confirmations + 1)?;
            let (s_tree, o_tree) = get_tree_state(&mut client, cp_height).await?;
            let p = Payment {
                recipients: vec![PaymentItem {
                    address,
                    amount,
                    memo: None,
                }],
            };
            let connection = zec.connection()?;
            let unsigned_tx = make_payment(
                network,
                &connection,
                account,
                cp_height,
                p,
                PoolMask(pools),
                fee_paid_by_sender != 0,
                &s_tree,
                &o_tree,
            )?;
            *txbytes = display_tx(
                network,
                &connection,
                cp_height,
                unsigned_tx,
                &mut TSKStore::default(),
            )?;
        }
        Command::GetTx { account, id } => {
            let connection = zec.connection()?;
            let (txid, timestamp) = get_txid(&connection, id)?;
            let mut client = zec.connect_lwd().await?;
            let (height, tx) = get_transaction(network, &mut client, &txid).await?;
            let tx = analyze_raw_transaction(
                network,
                &connection,
                zec.url.clone(),
                height,
                timestamp,
                account,
                tx,
            )?;
            let txb = serde_cbor::to_vec(&tx)?;
            println!("{}", hex::encode(&txb));
            store_tx_details(&connection, id, &tx.txid, &txb)?;
        }
        Command::GenDiversifiedAddress { account, pools } => {
            let connection = zec.connection()?;
            let time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as u32;
            let address =
                get_diversified_address(network, &connection, account, time, PoolMask(pools))?;
            println!("{}", address);
        }
        Command::Sweep {
            account,
            destination_address,
        } => {
            let connection = zec.connection()?;
            let ai = get_account_info(network, &connection, account)?;
            let mut client = zec.connect_lwd().await?;
            let bc_height = get_last_height(&mut client).await?;
            let cp_height = snap_to_checkpoint(&connection, bc_height - CONFIG.confirmations + 1)?;
            let (s, o) = get_tree_state(&mut client, cp_height).await?;
            let (utxos, mut tsk_store) =
                scan_utxo_by_seed(network, &zec.url, ai, bc_height, 0, true, 40).await?;
            let connection = zec.connection()?;
            let unsigned_tx = prepare_sweep(
                network,
                &connection,
                account,
                bc_height,
                &utxos,
                destination_address,
                &s,
                &o,
            )?;
            *txbytes = display_tx(network, &connection, cp_height, unsigned_tx, &mut tsk_store)?;
        }
        Command::GetTxDetails { id } => {
            let connection = zec.connection()?;
            let (account, tx) = get_tx_details(&connection, id)?;
            decode_tx_details(network, &connection, account, id, &tx)?;
            let etx = tx.to_transaction_info_ext(network);
            println!("{}", serde_json::to_string_pretty(&etx).unwrap());
        }
        Command::DecodeAddress { address } => {
            let ra = RecipientAddress::decode(network, &address)
                .ok_or(anyhow::anyhow!("Invalid Address"))?;
            println!("{:?}", ra);
        }
        Command::ListTxs { account } => {
            let mut client = zec.connect_lwd().await?;
            let bc_height = get_last_height(&mut client).await?;
            let connection = zec.connection()?;
            let txs = get_txs(network, &connection, account, bc_height)?;

            for tx in txs.iter() {
                println!("{}", serde_json::to_string_pretty(tx).unwrap());
            }
            let _data = fb_vec_to_bytes!(txs, TransactionInfo)?;
            // println!("{}", hex::encode(data));
        }
        Command::ListNotes { account } => {
            let mut client = zec.connect_lwd().await?;
            let bc_height = get_last_height(&mut client).await?;
            let connection = zec.connection()?;
            let notes = get_unspent_notes(&connection, account, bc_height)?;

            println!("{}", serde_json::to_string_pretty(&notes).unwrap());
            let _data = fb_vec_to_bytes!(notes, ShieldedNote)?;
        }
        Command::ListMessages { account } => {
            let connection = zec.connection()?;
            let msgs = list_messages(&connection, account)?;
            println!("{}", serde_json::to_string_pretty(&msgs).unwrap());
        }
        Command::DecodeUA { ua } => {
            let ua = decode_ua(network, &ua)?;
            println!("{}", serde_json::to_string_pretty(&ua).unwrap());
        }
        Command::MakePaymentURI { recipients } => {
            let recipients = recipients
                .iter()
                .map(|r| PaymentItem::try_from(r))
                .collect::<Result<Vec<_>, _>>()?;
            let payment_uri = make_payment_uri(&recipients)?;
            println!("{}", payment_uri);
        }
        Command::PayPaymentUri { account, uri } => {
            let recipients = parse_payment_uri(&uri)?;
            let mut client = zec.connect_lwd().await?;
            let bc_height = get_last_height(&mut client).await?;
            let connection = zec.connection()?;
            let cp_height = snap_to_checkpoint(&connection, bc_height - CONFIG.confirmations + 1)?;
            let (s, o) = get_tree_state(&mut client, cp_height).await?;
            let unsigned_tx = make_payment(
                network,
                &connection,
                account,
                cp_height,
                recipients,
                PoolMask(7),
                true,
                &s,
                &o,
            )?;
            *txbytes = display_tx(
                network,
                &connection,
                cp_height,
                unsigned_tx,
                &mut TSKStore::default(),
            )?;
        }
        Command::BroadcastLatest { clear } => {
            let clear = clear.unwrap_or(1);
            if clear != 0 {
                if !txbytes.is_empty() {
                    let mut client = zec.connect_lwd().await?;
                    let bc_height = get_last_height(&mut client).await?;
                    let r = broadcast(&mut client, bc_height, &txbytes).await?;
                    println!("{}", r);
                }
            }
        }
    }
    Ok(())
}

pub fn cli_main() -> Result<()> {
    let mut zec = CoinDef::from_network(zcash_primitives::consensus::Network::MainNetwork);
    zec.set_db_path(&CONFIG.db_path).unwrap();
    zec.set_url(&CONFIG.lwd_url);
    zec.set_warp(&CONFIG.warp_url);
    let prompt = DefaultPrompt {
        left_prompt: DefaultPromptSegment::Basic("zcash-warp".to_owned()),
        ..DefaultPrompt::default()
    };
    let rl = ClapEditor::<Command>::builder()
        .with_prompt(Box::new(prompt))
        .with_editor_hook(|reed| {
            reed.with_history(Box::new(
                FileBackedHistory::with_file(10000, "/tmp/zcash-warp-history".into()).unwrap(),
            ))
        })
        .build();

    let mut txbytes = vec![];
    rl.repl(|command| {
        if let Err(e) = process_command(command, &mut zec, &mut txbytes) {
            println!("{} {}", style("Error:").red().bold(), e);
        }
    });

    Ok(())
}

pub fn init_config() -> Config {
    let config: Config = Figment::new()
        .merge(Toml::file("App.toml"))
        .merge(Env::prefixed("ZCASH_WARP_"))
        .extract()
        .unwrap();
    config
}

lazy_static::lazy_static! {
    pub static ref CONFIG: Config = init_config();
}
