pub mod bitcoind_client;
mod cli;
mod convert;
mod disk;
mod hex_utils;

use crate::bitcoind_client::BitcoindClient;
use crate::disk::FilesystemLogger;
use bitcoin::blockdata::constants::genesis_block;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::BlockHash;
use bitcoin_bech32::WitnessProgram;
use lightning::chain;
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::chain::chainmonitor;
use lightning::chain::keysinterface::{InMemorySigner, KeysInterface, KeysManager};
use lightning::chain::Filter;
use lightning::chain::Watch;
use lightning::ln::channelmanager;
use lightning::ln::channelmanager::{
	BestBlock, ChainParameters, ChannelManagerReadArgs, SimpleArcChannelManager,
};
use lightning::ln::peer_handler::{MessageHandler, SimpleArcPeerManager};
use lightning::ln::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::routing::network_graph::NetGraphMsgHandler;
use lightning::util::config::UserConfig;
use lightning::util::events::{Event, EventsProvider};
use lightning::util::ser::ReadableArgs;
use lightning_background_processor::BackgroundProcessor;
use lightning_block_sync::init;
use lightning_block_sync::poll;
use lightning_block_sync::SpvClient;
use lightning_block_sync::UnboundedCache;
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::FilesystemPersister;
use rand::{thread_rng, Rng};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tokio::sync::mpsc::Receiver;

pub(crate) enum HTLCStatus {
	Pending,
	Succeeded,
	Failed,
}

pub(crate) struct MillisatAmount(Option<u64>);

impl fmt::Display for MillisatAmount {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self.0 {
			Some(amt) => write!(f, "{}", amt),
			None => write!(f, "unknown"),
		}
	}
}

pub(crate) struct PaymentInfo {
	preimage: Option<PaymentPreimage>,
	secret: Option<PaymentSecret>,
	status: HTLCStatus,
	amt_msat: MillisatAmount,
}

pub(crate) type PaymentInfoStorage = Arc<Mutex<HashMap<PaymentHash, PaymentInfo>>>;

type ChainMonitor = chainmonitor::ChainMonitor<
	InMemorySigner,
	Arc<dyn Filter + Send + Sync>,
	Arc<BitcoindClient>,
	Arc<BitcoindClient>,
	Arc<FilesystemLogger>,
	Arc<FilesystemPersister>,
>;

pub(crate) type PeerManager = SimpleArcPeerManager<
	SocketDescriptor,
	ChainMonitor,
	BitcoindClient,
	BitcoindClient,
	dyn chain::Access + Send + Sync,
	FilesystemLogger,
>;

pub(crate) type ChannelManager =
	SimpleArcChannelManager<ChainMonitor, BitcoindClient, BitcoindClient, FilesystemLogger>;

async fn handle_ldk_events(
	channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
	bitcoind_client: Arc<BitcoindClient>, keys_manager: Arc<KeysManager>,
	inbound_payments: PaymentInfoStorage, outbound_payments: PaymentInfoStorage, network: Network,
	mut event_receiver: Receiver<()>,
) {
	loop {
		let received = event_receiver.recv();
		if received.await.is_none() {
			println!("LDK Event channel closed!");
			return;
		}
		let loop_channel_manager = channel_manager.clone();
		let mut events = channel_manager.get_and_clear_pending_events();
		events.append(&mut chain_monitor.get_and_clear_pending_events());
		for event in events {
			match event {
				Event::FundingGenerationReady {
					temporary_channel_id,
					channel_value_satoshis,
					output_script,
					..
				} => {
					// Construct the raw transaction with one output, that is paid the amount of the
					// channel.
					let addr = WitnessProgram::from_scriptpubkey(
						&output_script[..],
						match network {
							Network::Bitcoin => bitcoin_bech32::constants::Network::Bitcoin,
							Network::Testnet => bitcoin_bech32::constants::Network::Testnet,
							Network::Regtest => bitcoin_bech32::constants::Network::Regtest,
							Network::Signet => panic!("Signet unsupported"),
						},
					)
					.expect("Lightning funding tx should always be to a SegWit output")
					.to_address();
					let mut outputs = vec![HashMap::with_capacity(1)];
					outputs[0].insert(addr, channel_value_satoshis as f64 / 100_000_000.0);
					let raw_tx = bitcoind_client.create_raw_transaction(outputs).await;

					// Have your wallet put the inputs into the transaction such that the output is
					// satisfied.
					let funded_tx = bitcoind_client.fund_raw_transaction(raw_tx).await;
					let change_output_position = funded_tx.changepos;
					assert!(change_output_position == 0 || change_output_position == 1);

					// Sign the final funding transaction and broadcast it.
					let signed_tx =
						bitcoind_client.sign_raw_transaction_with_wallet(funded_tx.hex).await;
					assert_eq!(signed_tx.complete, true);
					let final_tx: Transaction =
						encode::deserialize(&hex_utils::to_vec(&signed_tx.hex).unwrap()).unwrap();
					// Give the funding transaction back to LDK for opening the channel.
					loop_channel_manager
						.funding_transaction_generated(&temporary_channel_id, final_tx)
						.unwrap();
				}
				Event::PaymentReceived {
					payment_hash,
					payment_preimage,
					payment_secret,
					amt,
					..
				} => {
					let mut payments = inbound_payments.lock().unwrap();
					let status = match loop_channel_manager.claim_funds(payment_preimage.unwrap()) {
						true => {
							println!(
								"\nEVENT: received payment from payment hash {} of {} millisatoshis",
								hex_utils::hex_str(&payment_hash.0),
								amt
							);
							print!("> ");
							io::stdout().flush().unwrap();
							HTLCStatus::Succeeded
						}
						_ => HTLCStatus::Failed,
					};
					match payments.entry(payment_hash) {
						Entry::Occupied(mut e) => {
							let payment = e.get_mut();
							payment.status = status;
							payment.preimage = Some(payment_preimage.unwrap());
							payment.secret = Some(payment_secret);
						}
						Entry::Vacant(e) => {
							e.insert(PaymentInfo {
								preimage: Some(payment_preimage.unwrap()),
								secret: Some(payment_secret),
								status,
								amt_msat: MillisatAmount(Some(amt)),
							});
						}
					}
				}
				Event::PaymentSent { payment_preimage } => {
					let hashed = PaymentHash(Sha256::hash(&payment_preimage.0).into_inner());
					let mut payments = outbound_payments.lock().unwrap();
					for (payment_hash, payment) in payments.iter_mut() {
						if *payment_hash == hashed {
							payment.preimage = Some(payment_preimage);
							payment.status = HTLCStatus::Succeeded;
							println!(
								"\nEVENT: successfully sent payment of {} millisatoshis from \
                                         payment hash {:?} with preimage {:?}",
								payment.amt_msat,
								hex_utils::hex_str(&payment_hash.0),
								hex_utils::hex_str(&payment_preimage.0)
							);
							print!("> ");
							io::stdout().flush().unwrap();
						}
					}
				}
				Event::PaymentFailed { payment_hash, rejected_by_dest } => {
					print!(
						"\nEVENT: Failed to send payment to payment hash {:?}: ",
						hex_utils::hex_str(&payment_hash.0)
					);
					if rejected_by_dest {
						println!("rejected by destination node");
					} else {
						println!("route failed");
					}
					print!("> ");
					io::stdout().flush().unwrap();

					let mut payments = outbound_payments.lock().unwrap();
					if payments.contains_key(&payment_hash) {
						let payment = payments.get_mut(&payment_hash).unwrap();
						payment.status = HTLCStatus::Failed;
					}
				}
				Event::PendingHTLCsForwardable { time_forwardable } => {
					let forwarding_channel_manager = loop_channel_manager.clone();
					tokio::spawn(async move {
						let min = time_forwardable.as_millis() as u64;
						let millis_to_sleep = thread_rng().gen_range(min, min * 5) as u64;
						tokio::time::sleep(Duration::from_millis(millis_to_sleep)).await;
						forwarding_channel_manager.process_pending_htlc_forwards();
					});
				}
				Event::SpendableOutputs { outputs } => {
					let destination_address = bitcoind_client.get_new_address().await;
					let output_descriptors = &outputs.iter().map(|a| a).collect::<Vec<_>>();
					let tx_feerate =
						bitcoind_client.get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
					let spending_tx = keys_manager
						.spend_spendable_outputs(
							output_descriptors,
							Vec::new(),
							destination_address.script_pubkey(),
							tx_feerate,
							&Secp256k1::new(),
						)
						.unwrap();
					bitcoind_client.broadcast_transaction(&spending_tx);
				}
			}
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}
}

async fn start_ldk() {
	let args = match cli::parse_startup_args() {
		Ok(user_args) => user_args,
		Err(()) => return,
	};

	// Initialize the LDK data directory if necessary.
	let ldk_data_dir = format!("{}/.ldk", args.ldk_storage_dir_path);
	fs::create_dir_all(ldk_data_dir.clone()).unwrap();

	// Initialize our bitcoind client.
	let bitcoind_client = match BitcoindClient::new(
		args.bitcoind_rpc_host.clone(),
		args.bitcoind_rpc_port,
		args.bitcoind_rpc_username.clone(),
		args.bitcoind_rpc_password.clone(),
	)
	.await
	{
		Ok(client) => Arc::new(client),
		Err(e) => {
			println!("Failed to connect to bitcoind client: {}", e);
			return;
		}
	};

	// Check that the bitcoind we've connected to is running the network we expect
	let bitcoind_chain = bitcoind_client.get_blockchain_info().await.chain;
	if bitcoind_chain
		!= match args.network {
			bitcoin::Network::Bitcoin => "main",
			bitcoin::Network::Testnet => "test",
			bitcoin::Network::Regtest => "regtest",
			bitcoin::Network::Signet => "signet",
		} {
		println!(
			"Chain argument ({}) didn't match bitcoind chain ({})",
			args.network, bitcoind_chain
		);
		return;
	}

	// ## Setup
	// Step 1: Initialize the FeeEstimator

	// BitcoindClient implements the FeeEstimator trait, so it'll act as our fee estimator.
	let fee_estimator = bitcoind_client.clone();

	// Step 2: Initialize the Logger
	let logger = Arc::new(FilesystemLogger::new(ldk_data_dir.clone()));

	// Step 3: Initialize the BroadcasterInterface

	// BitcoindClient implements the BroadcasterInterface trait, so it'll act as our transaction
	// broadcaster.
	let broadcaster = bitcoind_client.clone();

	// Step 4: Initialize Persist
	let persister = Arc::new(FilesystemPersister::new(ldk_data_dir.clone()));

	// Step 5: Initialize the ChainMonitor
	let chain_monitor: Arc<ChainMonitor> = Arc::new(chainmonitor::ChainMonitor::new(
		None,
		broadcaster.clone(),
		logger.clone(),
		fee_estimator.clone(),
		persister.clone(),
	));

	// Step 6: Initialize the KeysManager

	// The key seed that we use to derive the node privkey (that corresponds to the node pubkey) and
	// other secret key material.
	let keys_seed_path = format!("{}/keys_seed", ldk_data_dir.clone());
	let keys_seed = if let Ok(seed) = fs::read(keys_seed_path.clone()) {
		assert_eq!(seed.len(), 32);
		let mut key = [0; 32];
		key.copy_from_slice(&seed);
		key
	} else {
		let mut key = [0; 32];
		thread_rng().fill_bytes(&mut key);
		match File::create(keys_seed_path.clone()) {
			Ok(mut f) => {
				f.write_all(&key).expect("Failed to write node keys seed to disk");
				f.sync_all().expect("Failed to sync node keys seed to disk");
			}
			Err(e) => {
				println!("ERROR: Unable to create keys seed file {}: {}", keys_seed_path, e);
				return;
			}
		}
		key
	};
	let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
	let keys_manager = Arc::new(KeysManager::new(&keys_seed, cur.as_secs(), cur.subsec_nanos()));

	// Step 7: Read ChannelMonitor state from disk
	let mut channelmonitors = persister.read_channelmonitors(keys_manager.clone()).unwrap();

	// Step 8: Initialize the ChannelManager
	let user_config = UserConfig::default();
	let mut restarting_node = true;
	let (channel_manager_blockhash, mut channel_manager) = {
		if let Ok(mut f) = fs::File::open(format!("{}/manager", ldk_data_dir.clone())) {
			let mut channel_monitor_mut_references = Vec::new();
			for (_, channel_monitor) in channelmonitors.iter_mut() {
				channel_monitor_mut_references.push(channel_monitor);
			}
			let read_args = ChannelManagerReadArgs::new(
				keys_manager.clone(),
				fee_estimator.clone(),
				chain_monitor.clone(),
				broadcaster.clone(),
				logger.clone(),
				user_config,
				channel_monitor_mut_references,
			);
			<(BlockHash, ChannelManager)>::read(&mut f, read_args).unwrap()
		} else {
			// We're starting a fresh node.
			restarting_node = false;
			let getinfo_resp = bitcoind_client.get_blockchain_info().await;

			let chain_params = ChainParameters {
				network: args.network,
				best_block: BestBlock::new(
					getinfo_resp.latest_blockhash,
					getinfo_resp.latest_height as u32,
				),
			};
			let fresh_channel_manager = channelmanager::ChannelManager::new(
				fee_estimator.clone(),
				chain_monitor.clone(),
				broadcaster.clone(),
				logger.clone(),
				keys_manager.clone(),
				user_config,
				chain_params,
			);
			(getinfo_resp.latest_blockhash, fresh_channel_manager)
		}
	};

	// Step 9: Sync ChannelMonitors and ChannelManager to chain tip
	let mut chain_listener_channel_monitors = Vec::new();
	let mut cache = UnboundedCache::new();
	let mut chain_tip: Option<poll::ValidatedBlockHeader> = None;
	if restarting_node {
		let mut chain_listeners =
			vec![(channel_manager_blockhash, &mut channel_manager as &mut dyn chain::Listen)];

		for (blockhash, channel_monitor) in channelmonitors.drain(..) {
			let outpoint = channel_monitor.get_funding_txo().0;
			chain_listener_channel_monitors.push((
				blockhash,
				(channel_monitor, broadcaster.clone(), fee_estimator.clone(), logger.clone()),
				outpoint,
			));
		}

		for monitor_listener_info in chain_listener_channel_monitors.iter_mut() {
			chain_listeners.push((
				monitor_listener_info.0,
				&mut monitor_listener_info.1 as &mut dyn chain::Listen,
			));
		}
		chain_tip = Some(
			init::synchronize_listeners(
				&mut bitcoind_client.deref(),
				args.network,
				&mut cache,
				chain_listeners,
			)
			.await
			.unwrap(),
		);
	}

	// Step 10: Give ChannelMonitors to ChainMonitor
	for item in chain_listener_channel_monitors.drain(..) {
		let channel_monitor = item.1 .0;
		let funding_outpoint = item.2;
		chain_monitor.watch_channel(funding_outpoint, channel_monitor).unwrap();
	}

	// Step 11: Optional: Initialize the NetGraphMsgHandler
	// XXX persist routing data
	let genesis = genesis_block(args.network).header.block_hash();
	let router = Arc::new(NetGraphMsgHandler::new(
		genesis,
		None::<Arc<dyn chain::Access + Send + Sync>>,
		logger.clone(),
	));

	// Step 12: Initialize the PeerManager
	let channel_manager: Arc<ChannelManager> = Arc::new(channel_manager);
	let mut ephemeral_bytes = [0; 32];
	rand::thread_rng().fill_bytes(&mut ephemeral_bytes);
	let lightning_msg_handler =
		MessageHandler { chan_handler: channel_manager.clone(), route_handler: router.clone() };
	let peer_manager: Arc<PeerManager> = Arc::new(PeerManager::new(
		lightning_msg_handler,
		keys_manager.get_node_secret(),
		&ephemeral_bytes,
		logger.clone(),
	));

	// ## Running LDK
	// Step 13: Initialize networking

	// We poll for events in handle_ldk_events(..) rather than waiting for them over the
	// mpsc::channel, so we can leave the event receiver as unused.
	let (event_ntfn_sender, event_ntfn_receiver) = mpsc::channel(2);
	let peer_manager_connection_handler = peer_manager.clone();
	let event_notifier = event_ntfn_sender.clone();
	let listening_port = args.ldk_peer_listening_port;
	tokio::spawn(async move {
		let listener = std::net::TcpListener::bind(format!("0.0.0.0:{}", listening_port)).unwrap();
		loop {
			let peer_mgr = peer_manager_connection_handler.clone();
			let notifier = event_notifier.clone();
			let tcp_stream = listener.accept().unwrap().0;
			tokio::spawn(async move {
				lightning_net_tokio::setup_inbound(peer_mgr.clone(), notifier.clone(), tcp_stream)
					.await;
			});
		}
	});

	// Step 14: Connect and Disconnect Blocks
	if chain_tip.is_none() {
		chain_tip =
			Some(init::validate_best_block_header(&mut bitcoind_client.deref()).await.unwrap());
	}
	let channel_manager_listener = channel_manager.clone();
	let chain_monitor_listener = chain_monitor.clone();
	let bitcoind_block_source = bitcoind_client.clone();
	let network = args.network;
	tokio::spawn(async move {
		let mut derefed = bitcoind_block_source.deref();
		let chain_poller = poll::ChainPoller::new(&mut derefed, network);
		let chain_listener = (chain_monitor_listener, channel_manager_listener);
		let mut spv_client =
			SpvClient::new(chain_tip.unwrap(), chain_poller, &mut cache, &chain_listener);
		loop {
			spv_client.poll_best_tip().await.unwrap();
			tokio::time::sleep(Duration::from_secs(1)).await;
		}
	});

	// Step 15: Initialize LDK Event Handling
	let channel_manager_event_listener = channel_manager.clone();
	let chain_monitor_event_listener = chain_monitor.clone();
	let keys_manager_listener = keys_manager.clone();
	// TODO: persist payment info to disk
	let inbound_payments: PaymentInfoStorage = Arc::new(Mutex::new(HashMap::new()));
	let outbound_payments: PaymentInfoStorage = Arc::new(Mutex::new(HashMap::new()));
	let inbound_pmts_for_events = inbound_payments.clone();
	let outbound_pmts_for_events = outbound_payments.clone();
	let network = args.network;
	let bitcoind_rpc = bitcoind_client.clone();
	tokio::spawn(async move {
		handle_ldk_events(
			channel_manager_event_listener,
			chain_monitor_event_listener,
			bitcoind_rpc,
			keys_manager_listener,
			inbound_pmts_for_events,
			outbound_pmts_for_events,
			network,
			event_ntfn_receiver,
		)
		.await;
	});

	// Step 16 & 17: Persist ChannelManager & Background Processing
	let data_dir = ldk_data_dir.clone();
	let persist_channel_manager_callback =
		move |node: &ChannelManager| FilesystemPersister::persist_manager(data_dir.clone(), &*node);
	BackgroundProcessor::start(
		persist_channel_manager_callback,
		channel_manager.clone(),
		peer_manager.clone(),
		logger.clone(),
	);

	// Reconnect to channel peers if possible.
	let peer_data_path = format!("{}/channel_peer_data", ldk_data_dir.clone());
	match disk::read_channel_peer_data(Path::new(&peer_data_path)) {
		Ok(mut info) => {
			for (pubkey, peer_addr) in info.drain() {
				for chan_info in channel_manager.list_channels() {
					if pubkey == chan_info.remote_network_id {
						let _ = cli::connect_peer_if_necessary(
							pubkey,
							peer_addr,
							peer_manager.clone(),
							event_ntfn_sender.clone(),
						);
					}
				}
			}
		}
		Err(e) => println!("ERROR: errored reading channel peer info from disk: {:?}", e),
	}

	// Start the CLI.
	cli::poll_for_user_input(
		peer_manager.clone(),
		channel_manager.clone(),
		keys_manager.clone(),
		router.clone(),
		inbound_payments,
		outbound_payments,
		event_ntfn_sender,
		ldk_data_dir.clone(),
		logger.clone(),
		args.network,
	)
	.await;
}

#[tokio::main]
pub async fn main() {
	start_ldk().await;
}
