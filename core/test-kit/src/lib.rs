use log::*;

use crate::eth_account::{parse_ether, EthereumAccount};
use crate::zksync_account::ZksyncAccount;
use bigdecimal::BigDecimal;
use eth_client::ETHClient;
use franklin_crypto::eddsa::{PrivateKey, PublicKey, Signature};
use futures::{
    channel::{mpsc, oneshot},
    compat::Future01CompatExt,
    executor::block_on,
    SinkExt, StreamExt, TryFutureExt,
};
use models::abi::FRANKLIN_CONTRACT;
use models::node::tx::TxSignature;
use models::node::{
    Account, AccountAddress, AccountMap, Engine, FranklinTx, Nonce, PriorityOp, TokenId, Transfer,
};
use models::CommitRequest;
use rand::{Rng, SeedableRng, XorShiftRng};
use server::mempool::ProposedBlock;
use server::state_keeper::{
    start_state_keeper, PlasmaStateInitParams, PlasmaStateKeeper, StateKeeperRequest,
};
use server::ConfigurationOptions;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::str::FromStr;
use std::sync::mpsc::channel;
use std::time::Duration;
use storage::ConnectionPool;
use tokio::runtime::Runtime;
use tokio::spawn;
use web3::contract::{Contract, Options};
use web3::transports::{EventLoopHandle, Http};
use web3::types::{Address, H256, U256};
use web3::Transport;

pub mod eth_account;
pub mod zksync_account;

struct AccountSet<T: Transport> {
    eth_accounts: Vec<EthereumAccount<T>>,
    zksync_accounts: Vec<ZksyncAccount>,
}

type ETHAccountSetId = usize;
type ZKSyncAccountSetId = usize;

enum AccountSetId {
    ETHAccount(ETHAccountSetId),
    ZKSync(ZKSyncAccountSetId),
}

impl<T: Transport> AccountSet<T> {
    fn deposit(
        &self,
        from: ETHAccountSetId,
        to: ZKSyncAccountSetId,
        token_id: TokenId,
        amount: BigDecimal,
        fee: BigDecimal,
    ) -> PriorityOp {
        let from = &self.eth_accounts[from];
        let to = &self.zksync_accounts[to];

        block_on(from.deposit_eth(amount, fee, &to.address)).expect("deposit should not fail")
    }

    fn transfer(
        &self,
        from: ZKSyncAccountSetId,
        to: ZKSyncAccountSetId,
        token_id: TokenId,
        amount: BigDecimal,
        fee: BigDecimal,
        nonce: Nonce,
    ) -> FranklinTx {
        let from = &self.zksync_accounts[from];
        let to = &self.zksync_accounts[to];

        FranklinTx::Transfer(from.sign_transfer(token_id, amount, fee, &to.address, nonce))
    }

    fn withdraw(
        &self,
        from: ZKSyncAccountSetId,
        to: ETHAccountSetId,
        token_id: TokenId,
        amount: BigDecimal,
        fee: BigDecimal,
        nonce: Nonce,
    ) -> FranklinTx {
        let from = &self.zksync_accounts[from];
        let to = &self.eth_accounts[to];

        FranklinTx::Withdraw(from.sign_withdraw(token_id, amount, fee, &to.address, nonce))
    }
}

struct TestBlock {
    transactions: Vec<FranklinTx>,
}

fn gen_pk() -> PrivateKey<Engine> {
    let mut rng = XorShiftRng::from_seed([1, 2, 3, 4]);

    PrivateKey(rng.gen())
}

fn new_transfer() -> FranklinTx {
    let pk = gen_pk();
    let mut transfer = Transfer {
        from: AccountAddress::default(),
        to: AccountAddress::default(),
        token: 0,
        amount: BigDecimal::from(0),
        fee: BigDecimal::from(0),
        nonce: 0,
        signature: TxSignature::default(),
    };
    transfer.signature = TxSignature::sign_musig_pedersen(&pk, &transfer.get_bytes());
    FranklinTx::Transfer(transfer)
}

fn genesis_state(config_opts: &ConfigurationOptions) -> PlasmaStateInitParams {
    let mut accounts = AccountMap::default();
    let operator_account = Account::default_with_address(&config_opts.operator_franklin_addr);
    accounts.insert(0, operator_account);

    PlasmaStateInitParams {
        accounts,
        last_block_number: 0,
        unprocessed_priority_op: 0,
    }
}

fn dummy_proposed_block() -> ProposedBlock {
    ProposedBlock {
        priority_ops: Vec::new(),
        txs: vec![new_transfer()],
    }
}

async fn sk_get_account(
    mut sender: mpsc::Sender<StateKeeperRequest>,
    address: &AccountAddress,
) -> Account {
    let resp = oneshot::channel();
    sender
        .send(StateKeeperRequest::GetAccount(address.clone(), resp.0))
        .await
        .expect("sk request send");
    resp.1
        .await
        .expect("sk account resp recv")
        .unwrap_or_else(|| Account::default_with_address(address))
}

pub fn init_and_run_state_keeper() {
    let connection_pool = ConnectionPool::new();
    let config = ConfigurationOptions::from_env();

    let (proposed_blocks_sender, mut proposed_blocks_receiver) = mpsc::channel(256);
    let (state_keeper_req_sender, state_keeper_req_receiver) = mpsc::channel(256);
    let (executed_tx_notify_sender, executed_tx_notify_receiver) = mpsc::channel(256);

    let state_keeper = PlasmaStateKeeper::new(
        genesis_state(&config),
        config.operator_franklin_addr.clone(),
        state_keeper_req_receiver,
        proposed_blocks_sender,
        executed_tx_notify_sender,
    );

    let (mut stop_state_keeper_sender, stop_state_keeper_receiver) = oneshot::channel::<()>();
    let sk_thread_handle = std::thread::spawn(move || {
        let mut main_runtime = Runtime::new().expect("main runtime start");
        start_state_keeper(state_keeper, &main_runtime);
        main_runtime.block_on(async move {
            stop_state_keeper_receiver.await;
        })
    });

    //    let state_proxy = StateProxy::new(&config, state_keeper_req_sender.clone());
    let (_el, transport) = Http::new(&config.web3_url).expect("http transport start");

    let commit_account = EthereumAccount::new(
        config.operator_private_key.clone(),
        config.operator_eth_addr.clone(),
        transport.clone(),
        &config,
    );

    let eth_account = EthereumAccount::new(
        config.operator_private_key,
        config.operator_eth_addr,
        transport,
        &config,
    );
    let zksync_account1 = ZksyncAccount::rand();
    let zksync_account2 = ZksyncAccount::rand();

    let accounts = AccountSet {
        eth_accounts: vec![eth_account],
        zksync_accounts: vec![zksync_account1, zksync_account2],
    };

    //    let mut expected_balances = HashMap::new();

    let deposit = accounts.deposit(
        0,
        0,
        0,
        parse_ether("1.0").unwrap(),
        parse_ether("0.1").unwrap(),
    );
    let transfer1 = accounts.transfer(
        0,
        1,
        0,
        parse_ether("0.25").unwrap(),
        BigDecimal::from(0),
        0,
    );
    //    let transfer2 = accounts.transfer(0, 1, 0, parse_ether("0.25").unwrap(), BigDecimal::from(0), 1);
    let withdraw = accounts.withdraw(0, 0, 0, parse_ether("0.5").unwrap(), BigDecimal::from(0), 1);

    //    let mut eth_acc_balance = block_on(eth_account.eth_balance()).expect("eth balance get");
    //
    //    let deposit_amount = parse_ether("1").unwrap();
    //    eth_acc_balance -= &deposit_amount;
    //    let res = block_on(eth_account.deposit_eth(
    //        deposit_amount.clone(),
    //        BigDecimal::from(0),
    //        &zksync_account.address,
    //    ))
    //    .expect("deposit fail");

    //    let transfer_amount = parse_ether("0.33").unwrap();
    //    let mut zksync_balance = deposit_amount;
    //    zksync_balance -= &transfer_amount;
    //    let transfer = FranklinTx::Transfer(zksync_account.sign_transfer(
    //        0,
    //        transfer_amount,
    //        BigDecimal::from(0),
    //        &AccountAddress::default(),
    //        0,
    //    ));

    let block = ProposedBlock {
        priority_ops: vec![deposit],
        txs: vec![transfer1, withdraw],
    };

    let empty_block = async {
        state_keeper_req_sender
            .clone()
            .send(StateKeeperRequest::ExecuteMiniBlock(block))
            .await;
        state_keeper_req_sender
            .clone()
            .send(StateKeeperRequest::SealBlock)
            .await;
    };

    block_on(empty_block);

    let mut next_block = || -> CommitRequest {
        block_on(async {
            if let Some(op) = proposed_blocks_receiver.next().await {
                println!("op: {:#?}", op);
                return op;
            } else {
                panic!("State keeper channel closed");
            }
        })
    };

    let new_block = next_block();
    // commit block on eth
    println!("commiting new block to eth");
    let block_rec = block_on(commit_account.commit_block(&new_block.block));
    //    println!("block receipt: {:#?}", block_rec);
    let before_ver = block_on(accounts.eth_accounts[0].eth_balance()).expect("eth balance get");
    let block_rec = block_on(commit_account.verify_block(&new_block.block));
    let after_ver = block_on(accounts.eth_accounts[0].eth_balance()).expect("eth balance get");
    println!("diff {}", after_ver - before_ver);
    println!("block receipt: {:#?}", block_rec);

    //

    //    // check
    //    let eth_acc_new_balance = block_on(eth_account.eth_balance()).expect("eth balance get");
    //    let zksync_acc_new_balance = block_on(sk_get_account(
    //        state_keeper_req_sender.clone(),
    //        &zksync_account.address,
    //    ))
    //    .get_balance(0);
    //
    //    println!("eth bal: {}", eth_acc_balance - eth_acc_new_balance);
    //    println!(
    //        "zksync bal: {} - {}",
    //        zksync_balance, zksync_acc_new_balance
    //    );

    stop_state_keeper_sender.send(());

    sk_thread_handle.join().expect("sk thread join");
}

struct StateProxy {
    web3_event_loop: EventLoopHandle,
    web3_transport: Http,
    state_keeper_request_sender: mpsc::Sender<StateKeeperRequest>,
    //    gov_contract: (ethabi::Contract, Contract<Http>),
    //    priority_queue_contract: (ethabi::Contract, Contract<Http>),
    //    main_contract: (ethabi::Contract, Contract<Http>),
    config: ConfigurationOptions,
}

impl StateProxy {
    fn new(
        config: &ConfigurationOptions,
        state_keeper_request_sender: mpsc::Sender<StateKeeperRequest>,
    ) -> Self {
        let (eloop, transport) = Http::new(&config.web3_url).expect("web3 transport");
        Self {
            web3_event_loop: eloop,
            web3_transport: transport,
            state_keeper_request_sender,
            config: config.clone(),
        }
    }

    fn emergency_withdraw(&self) {
        unimplemented!()
    }

    fn get_offhcain_balance(&self, address: &AccountAddress, token: String) -> BigDecimal {
        // ask state keeper
        unimplemented!()
    }

    fn get_onchain_balance(&self, address: &Address, token: String) -> BigDecimal {
        // ask ethereum
        unimplemented!()
    }
}
