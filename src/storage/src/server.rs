// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! An interactive dataflow server.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::anyhow;
use crossbeam_channel::TryRecvError;
use timely::communication::initialize::WorkerGuards;
use timely::communication::Allocate;
use timely::worker::Worker as TimelyWorker;
use tokio::sync::mpsc;

use mz_dataflow_types::client::{LocalClient, LocalStorageClient, StorageCommand, StorageResponse};
use mz_dataflow_types::ConnectorContext;
use mz_ore::metrics::MetricsRegistry;
use mz_ore::now::NowFn;

use crate::source::metrics::SourceBaseMetrics;
use crate::storage_state::ActiveStorageState;
use crate::storage_state::StorageState;
use crate::DecodeMetrics;

/// Configures a dataflow server.
pub struct Config {
    /// The number of worker threads to spawn.
    pub workers: usize,
    /// The Timely configuration
    pub timely_config: timely::Config,
    /// Whether the server is running in experimental mode.
    pub experimental_mode: bool,
    /// Function to get wall time now.
    pub now: NowFn,
    /// Metrics registry through which dataflow metrics will be reported.
    pub metrics_registry: MetricsRegistry,
    /// Configuration for source and sink connectors.
    pub connector_context: ConnectorContext,
}

/// A handle to a running dataflow server.
///
/// Dropping this object will block until the dataflow computation ceases.
pub struct Server {
    _worker_guards: WorkerGuards<()>,
}

/// Initiates a timely dataflow computation, processing materialized commands.
pub fn serve(config: Config) -> Result<(Server, LocalStorageClient), anyhow::Error> {
    assert!(config.workers > 0);

    // Various metrics related things.
    let source_metrics = SourceBaseMetrics::register_with(&config.metrics_registry);
    let decode_metrics = DecodeMetrics::register_with(&config.metrics_registry);
    // Bundle metrics to conceal complexity.
    let metrics_bundle = (source_metrics, decode_metrics);

    // Construct endpoints for each thread that will receive the coordinator's
    // sequenced command stream and send the responses to the coordinator.
    //
    // TODO(benesch): package up this idiom of handing out ownership of N items
    // to the N timely threads that will be spawned. The Mutex<Vec<Option<T>>>
    // is hard to read through.
    let (command_txs, command_rxs): (Vec<_>, Vec<_>) = (0..config.workers)
        .map(|_| crossbeam_channel::unbounded())
        .unzip();
    let (storage_response_txs, storage_response_rxs): (Vec<_>, Vec<_>) = (0..config.workers)
        .map(|_| mpsc::unbounded_channel())
        .unzip();
    // Mutexes around a vector of optional (take-able) pairs of (tx, rx) for worker/client communication.
    let command_channels: Mutex<Vec<_>> = Mutex::new(command_rxs.into_iter().map(Some).collect());
    let storage_response_channels: Mutex<Vec<_>> =
        Mutex::new(storage_response_txs.into_iter().map(Some).collect());

    let tokio_executor = tokio::runtime::Handle::current();
    let now = config.now;

    let worker_guards = timely::execute::execute(config.timely_config, move |timely_worker| {
        let timely_worker_index = timely_worker.index();
        let timely_worker_peers = timely_worker.peers();

        // ensure tokio primitives are available on timely workers
        let _tokio_guard = tokio_executor.enter();

        let command_rx = command_channels.lock().unwrap()[timely_worker_index % config.workers]
            .take()
            .unwrap();
        let storage_response_tx = storage_response_channels.lock().unwrap()
            [timely_worker_index % config.workers]
            .take()
            .unwrap();
        let (source_metrics, decode_metrics) = metrics_bundle.clone();
        Worker {
            timely_worker,
            command_rx,
            storage_state: StorageState {
                source_descriptions: HashMap::new(),
                source_uppers: HashMap::new(),
                persist_handles: HashMap::new(),
                collection_metadata: HashMap::new(),
                source_tokens: HashMap::new(),
                ts_source_mapping: HashMap::new(),
                decode_metrics,
                reported_frontiers: HashMap::new(),
                last_bindings_feedback: Instant::now(),
                now: now.clone(),
                source_metrics,
                timely_worker_index,
                timely_worker_peers,
                connector_context: config.connector_context.clone(),
            },
            storage_response_tx,
        }
        .run()
    })
    .map_err(|e| anyhow!("{}", e))?;
    let worker_threads = worker_guards
        .guards()
        .iter()
        .map(|g| g.thread().clone())
        .collect::<Vec<_>>();
    let storage_client = LocalClient::new(storage_response_rxs, command_txs, worker_threads);
    let server = Server {
        _worker_guards: worker_guards,
    };
    Ok((server, storage_client))
}

/// State maintained for each worker thread.
///
/// Much of this state can be viewed as local variables for the worker thread,
/// holding state that persists across function calls.
struct Worker<'w, A: Allocate> {
    /// The underlying Timely worker.
    timely_worker: &'w mut TimelyWorker<A>,
    /// The channel from which commands are drawn.
    command_rx: crossbeam_channel::Receiver<StorageCommand>,
    /// The state associated with collection ingress and egress.
    storage_state: StorageState,
    /// The channel over which storage responses are reported.
    storage_response_tx: mpsc::UnboundedSender<StorageResponse>,
}

impl<'w, A: Allocate> Worker<'w, A> {
    /// Draws from `dataflow_command_receiver` until shutdown.
    fn run(&mut self) {
        let mut shutdown = false;
        while !shutdown {
            // Ask Timely to execute a unit of work. If Timely decides there's
            // nothing to do, it will park the thread. We rely on another thread
            // unparking us when there's new work to be done, e.g., when sending
            // a command or when new Kafka messages have arrived.
            self.timely_worker.step_or_park(None);

            self.activate_storage().report_frontier_progress();

            // Handle any received commands.
            let mut cmds = vec![];
            let mut empty = false;
            while !empty {
                match self.command_rx.try_recv() {
                    Ok(cmd) => cmds.push(cmd),
                    Err(TryRecvError::Empty) => empty = true,
                    Err(TryRecvError::Disconnected) => {
                        empty = true;
                        shutdown = true;
                    }
                }
            }
            for cmd in cmds {
                self.activate_storage().handle_storage_command(cmd);
            }
        }
    }

    fn activate_storage(&mut self) -> ActiveStorageState<A> {
        ActiveStorageState {
            timely_worker: &mut *self.timely_worker,
            storage_state: &mut self.storage_state,
            response_tx: &mut self.storage_response_tx,
        }
    }
}
