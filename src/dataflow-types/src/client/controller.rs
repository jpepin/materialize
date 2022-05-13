// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! A representative of STORAGE and COMPUTE that maintains summaries of the involved objects.
//!
//! The `Controller` provides the ability to create and manipulate storage and compute instances.
//! Each of Storage and Compute provide their own controllers, accessed through the `storage()`
//! and `compute(instance_id)` methods. It is an error to access a compute instance before it has
//! been created; a single storage instance is always available.
//!
//! The controller also provides a `recv()` method that returns responses from the storage and
//! compute layers, which may remain of value to the interested user. With time, these responses
//! may be thinned down in an effort to make the controller more self contained.
//!
//! Consult the `StorageController` and `ComputeController` documentation for more information
//! about each of these interfaces.

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroUsize;

use derivative::Derivative;
use differential_dataflow::lattice::Lattice;
use futures::future::Either;
use futures::stream::{FuturesUnordered, StreamExt};
use maplit::hashmap;
use serde::{Deserialize, Serialize};
use timely::progress::frontier::{Antichain, AntichainRef};
use timely::progress::Timestamp;

use mz_orchestrator::{CpuLimit, MemoryLimit, Orchestrator, ServiceConfig, ServicePort};
use mz_persist_types::Codec64;

use crate::client::{
    ComputeClient, ComputeCommand, ComputeInstanceId, ComputeResponse,
    ConcreteComputeInstanceReplicaConfig, ControllerResponse, RemoteClient, ReplicaId,
    StorageResponse, Recv, GenericClient, Response,
};
use crate::logging::LoggingConfig;
use crate::{TailBatch, TailResponse};

pub use storage::{StorageController, StorageControllerState};
pub mod storage;
pub use compute::{ComputeController, ComputeControllerMut, ComputeControllerState};
mod compute;

/// Configures an orchestrator for the controller.
pub struct OrchestratorConfig {
    /// The orchestrator implementation to use.
    pub orchestrator: Box<dyn Orchestrator>,
    /// The computed image to use when starting new compute instances.
    pub computed_image: String,
    /// The storage address that compute instances should connect to.
    pub storage_addr: String,
    /// Whether or not process should die when connection with ADAPTER is lost.
    pub linger: bool,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct ClusterReplicaSizeConfig {
    pub memory_limit: Option<MemoryLimit>,
    pub cpu_limit: Option<CpuLimit>,
    pub scale: NonZeroUsize,
    pub workers: NonZeroUsize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ClusterReplicaSizeMap(pub HashMap<String, ClusterReplicaSizeConfig>);

impl Default for ClusterReplicaSizeMap {
    fn default() -> Self {
        // {
        //     "1": {"scale": 1, "workers": 1},
        //     "2": {"scale": 1, "workers": 2},
        //     "4": {"scale": 1, "workers": 4},
        //     /// ...
        //     "32": {"scale": 1, "workers": 32}
        //     /// Testing with multiple processes on a single machine is a novelty, so
        //     /// we don't bother providing many options.
        //     "2-1": {"scale": 2, "workers": 1},
        //     "2-2": {"scale": 2, "workers": 2},
        //     "2-4": {"scale": 2, "workers": 4},
        // }
        let mut inner = (0..=5)
            .map(|i| {
                let workers = 1 << i;
                (
                    workers.to_string(),
                    ClusterReplicaSizeConfig {
                        memory_limit: None,
                        cpu_limit: None,
                        scale: NonZeroUsize::new(1).unwrap(),
                        workers: NonZeroUsize::new(workers).unwrap(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        inner.insert(
            "2-1".to_string(),
            ClusterReplicaSizeConfig {
                memory_limit: None,
                cpu_limit: None,
                scale: NonZeroUsize::new(2).unwrap(),
                workers: NonZeroUsize::new(1).unwrap(),
            },
        );
        inner.insert(
            "2-2".to_string(),
            ClusterReplicaSizeConfig {
                memory_limit: None,
                cpu_limit: None,
                scale: NonZeroUsize::new(2).unwrap(),
                workers: NonZeroUsize::new(2).unwrap(),
            },
        );
        inner.insert(
            "2-4".to_string(),
            ClusterReplicaSizeConfig {
                memory_limit: None,
                cpu_limit: None,
                scale: NonZeroUsize::new(2).unwrap(),
                workers: NonZeroUsize::new(4).unwrap(),
            },
        );
        Self(inner)
    }
}

/// Deterministically generates replica names based on inputs.
///
/// This function needs to be publicly accessible so other layers can ensure we
/// do not attempt to create multiple replicas with the same name.
fn generate_replica_service_name(instance_id: ComputeInstanceId, replica_id: ReplicaId) -> String {
    format!("cluster-{instance_id}-replica-{replica_id}")
}

/// A client that maintains soft state and validates commands, in addition to forwarding them.
///
/// NOTE(benesch): I find the fact that this type is called `Controller` but is
/// referred to as the `dataflow_client` in the coordinator to be very
/// confusing. We should find the one correct name, and use it everywhere!
pub struct Controller<T = mz_repr::Timestamp> {
    orchestrator: OrchestratorConfig,
    storage_controller: Box<dyn StorageController<Timestamp = T>>,
    compute: BTreeMap<ComputeInstanceId, ComputeControllerState<T>>,
    storage_alive: bool,
}

impl<T> Controller<T>
where
    T: Timestamp + Lattice + Codec64 + Copy + Unpin,
{
    pub async fn create_instance(
        &mut self,
        instance: ComputeInstanceId,
        logging: Option<LoggingConfig>,
    ) -> Result<(), anyhow::Error> {
        // Insert a new compute instance controller.
        self.compute.insert(
            instance,
            ComputeControllerState::new(&logging).await?,
        );

        Ok(())
    }

    /// Adds replicas of an instance.
    ///
    /// # Panics
    /// - If the identified `instance` has not yet been created via
    ///   [`Self::create_instance`].
    pub async fn add_replica_to_instance(
        &mut self,
        instance_id: ComputeInstanceId,
        replica_id: ReplicaId,
        config: ConcreteComputeInstanceReplicaConfig,
    ) -> Result<(), anyhow::Error> {
        assert!(
            self.compute.contains_key(&instance_id),
            "call Controller::create_instance before calling add_replica_to_instance"
        );

        let replica_name = generate_replica_service_name(instance_id, replica_id);

        // Add replicas backing that instance.
        match config {
            ConcreteComputeInstanceReplicaConfig::Remote { replicas } => {
                let mut compute_instance = self.compute_mut(instance_id).unwrap();
                let client = RemoteClient::new(&replicas.into_iter().collect::<Vec<_>>());
                let client: Box<dyn ComputeClient<T>> = Box::new(client);
                compute_instance.add_replica(replica_name, client).await;
            }
            ConcreteComputeInstanceReplicaConfig::Managed {
                size_config,
                availability_zone,
            } => {
                let OrchestratorConfig {
                    orchestrator,
                    computed_image,
                    storage_addr,
                    linger,
                } = &self.orchestrator;

                let default_listen_host = orchestrator.listen_host();

                let service =
                    orchestrator
                        .namespace("compute")
                        .ensure_service(
                            &replica_name,
                            ServiceConfig {
                                image: computed_image.clone(),
                                args: &|hosts_ports, my_ports, my_index| {
                                    let mut compute_opts = vec![
                                        format!("--storage-addr={storage_addr}"),
                                        format!(
                                            "--listen-addr={}:{}",
                                            default_listen_host, my_ports["controller"]
                                        ),
                                        format!(
                                            "--http-console-addr={}:{}",
                                            default_listen_host, my_ports["http"]
                                        ),
                                        format!("--processes={}", size_config.scale),
                                        format!("--workers={}", size_config.workers),
                                        "--log-process-name".to_string(),
                                    ];
                                    compute_opts.extend(hosts_ports.iter().map(|(host, ports)| {
                                        format!("{host}:{}", ports["compute"])
                                    }));
                                    if let Some(my_index) = my_index {
                                        compute_opts.push(format!("--process={my_index}"));
                                    }
                                    if *linger {
                                        compute_opts.push(format!("--linger"));
                                    }
                                    compute_opts
                                },
                                ports: vec![
                                    ServicePort {
                                        name: "controller".into(),
                                        port_hint: 2100,
                                    },
                                    ServicePort {
                                        name: "compute".into(),
                                        port_hint: 2102,
                                    },
                                    ServicePort {
                                        name: "http".into(),
                                        port_hint: 2103,
                                    },
                                ],
                                cpu_limit: size_config.cpu_limit,
                                memory_limit: size_config.memory_limit,
                                scale: size_config.scale,
                                labels: hashmap! {
                                    "cluster-id".into() => instance_id.to_string(),
                                    "type".into() => "cluster".into(),
                                },
                                availability_zone,
                            },
                        )
                        .await?;
                let client = RemoteClient::new(&service.addresses("controller"));
                let client: Box<dyn ComputeClient<T>> = Box::new(client);
                self.compute_mut(instance_id)
                    .unwrap()
                    .add_replica(replica_name, client)
                    .await;
            }
        }

        Ok(())
    }

    /// Removes a replica from an instance, including its service in the
    /// orchestrator.
    pub async fn drop_replica(
        &mut self,
        instance_id: ComputeInstanceId,
        replica_id: ReplicaId,
        config: ConcreteComputeInstanceReplicaConfig,
    ) -> Result<(), anyhow::Error> {
        let replica_name = generate_replica_service_name(instance_id, replica_id);
        if let ConcreteComputeInstanceReplicaConfig::Managed {
            size_config: _size_config,
            availability_zone: _az,
        } = config
        {
            let OrchestratorConfig { orchestrator, .. } = &self.orchestrator;
            orchestrator
                .namespace("compute")
                .drop_service(&replica_name)
                .await?;
        }
        let mut compute = self.compute_mut(instance_id).unwrap();
        compute.remove_replica(&replica_name);
        Ok(())
    }

    /// Removes an instance from the orchestrator.
    ///
    /// # Panics
    /// - If the identified `instance` still has active replicas.
    pub async fn drop_instance(
        &mut self,
        instance: ComputeInstanceId,
    ) -> Result<(), anyhow::Error> {
        if let Some(mut compute) = self.compute.remove(&instance) {
            assert!(
                compute.client.get_replica_identifiers().next().is_none(),
                "cannot drop instances with provisioned replicas; call `drop_replica` first"
            );
            self.orchestrator
                .orchestrator
                .namespace("compute")
                .drop_service(&format!("cluster-{instance}"))
                .await?;
            compute.client.send(ComputeCommand::DropInstance).await?;
        }
        Ok(())
    }
}

impl<T> Controller<T> {
    /// Acquires an immutable handle to a controller for the storage instance.
    #[inline]
    pub fn storage(&self) -> &dyn StorageController<Timestamp = T> {
        &*self.storage_controller
    }

    /// Acquires a mutable handle to a controller for the storage instance.
    #[inline]
    pub fn storage_mut(&mut self) -> &mut dyn StorageController<Timestamp = T> {
        &mut *self.storage_controller
    }

    /// Acquires an immutable handle to a controller for the indicated compute instance, if it exists.
    #[inline]
    pub fn compute(&self, instance: ComputeInstanceId) -> Option<ComputeController<T>> {
        let compute = self.compute.get(&instance)?;
        Some(ComputeController {
            _instance: instance,
            compute,
            storage_controller: self.storage(),
        })
    }

    /// Acquires a mutable handle to a controller for the indicated compute instance, if it exists.
    #[inline]
    pub fn compute_mut(&mut self, instance: ComputeInstanceId) -> Option<ComputeControllerMut<T>> {
        let compute = self.compute.get_mut(&instance)?;
        Some(ComputeControllerMut {
            instance,
            compute,
            storage_controller: &mut *self.storage_controller,
        })
    }
}

impl<T> Controller<T>
where
    T: Timestamp + Lattice + Codec64,
{
    pub async fn recv(
        &mut self,
    ) -> Recv<'_, ControllerResponse<T>> {
        enum Res<'a, T> {
            Compute(i64, Recv<'a, ComputeResponse<T>>),
            Storage(Recv<'a, StorageResponse<T>>),
        }

        let mut recvs = FuturesUnordered::new();
        for (id, compute) in &mut self.compute {
            recvs.push(Either::Left(async {
                Res::Compute(*id, compute.client.recv().await)
            }));
        }
        if self.storage_alive {
            recvs.push(Either::Right(async {
                Res::Storage(self.storage_controller.recv().await)
            }));
        }

        let response = recvs.next().await;
        drop(recvs);
        Box::pin(async {
            match response {
                None => return Ok(Response::Done),
                Some(Res::Compute(instance, response)) => {
                    match response.await? {
                        Response::Ready(ComputeResponse::FrontierUppers(updates)) => {
                            self.compute_mut(instance)
                                // TODO: determine if this is an error, or perhaps just a late
                                // response about a terminated instance.
                                .expect("Reference to absent instance")
                                .update_write_frontiers(&updates)
                                .await?;
                            Ok(Response::Ready(ControllerResponse::RecvAgain))
                        }
                        Response::Ready(ComputeResponse::PeekResponse(uuid, response)) => {
                            self.compute_mut(instance)
                                .expect("Reference to absent instance")
                                .remove_peeks(std::iter::once(uuid))
                                .await?;
                            Ok(Response::Ready(ControllerResponse::PeekResponse(uuid, response)))
                        }
                        Response::Ready(ComputeResponse::TailResponse(global_id, response)) => {
                            let mut changes = timely::progress::ChangeBatch::new();
                            match &response {
                                TailResponse::Batch(TailBatch { lower, upper, .. }) => {
                                    changes.extend(upper.iter().map(|time| (time.clone(), 1)));
                                    changes.extend(lower.iter().map(|time| (time.clone(), -1)));
                                }
                                TailResponse::DroppedAt(frontier) => {
                                    // The tail will not be written to again, but we should not confuse that
                                    // with the source of the TAIL being complete through this time.
                                    changes.extend(frontier.iter().map(|time| (time.clone(), -1)));
                                }
                            }
                            self.compute_mut(instance)
                                .expect("Reference to absent instance")
                                .update_write_frontiers(&[(global_id, changes)])
                                .await?;
                            Ok(Response::Ready(ControllerResponse::TailResponse(global_id, response)))
                        }
                        Response::RecvAgain => Ok(Response::RecvAgain),
                        Response::Done => Ok(Response::Done),
                    }
                }
                Some(Res::Storage(response)) => {
                    match response.await? {
                        Response::Ready(StorageResponse::TimestampBindings(feedback)) => {
                            // Order is important here. We must durably record
                            // the timestamp bindings before we act on them, or
                            // an ill-timed crash could cause data loss.
                            self.storage_controller
                                .persist_timestamp_bindings(&feedback)
                                .await?;

                            self.storage_controller
                                .update_write_frontiers(&feedback.changes)
                                .await?;

                            Ok(Response::Ready(ControllerResponse::RecvAgain))
                        }
                        Response::Ready(StorageResponse::LinearizedTimestamps(res)) => {
                            Ok(Response::Ready(ControllerResponse::LinearizedTimestamps(res)))
                        }
                        Response::RecvAgain => Ok(Response::RecvAgain),
                        Response::Done => {
                            self.storage_alive = false;
                            Ok(Response::Ready(ControllerResponse::RecvAgain))
                        }
                    }
                }
            }
        })
    }
}

impl<T> Controller<T> {
    /// Create a new controller from a client it should wrap.
    pub fn new<S: StorageController<Timestamp = T> + 'static>(
        orchestrator: OrchestratorConfig,
        storage_controller: S,
    ) -> Self {
        Self {
            orchestrator,
            storage_controller: Box::new(storage_controller),
            compute: BTreeMap::default(),
            storage_alive: true,
        }
    }
}

use std::sync::Arc;

/// Compaction policies for collections maintained by `Controller`.
#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub enum ReadPolicy<T> {
    /// Maintain the collection as valid from this frontier onward.
    ValidFrom(Antichain<T>),
    /// Maintain the collection as valid from a function of the write frontier.
    ///
    /// This function will only be re-evaluated when the write frontier changes.
    /// If the intended behavior is to change in response to external signals,
    /// consider using the `ValidFrom` variant to manually pilot compaction.
    ///
    /// The `Arc` makes the function cloneable.
    LagWriteFrontier(
        #[derivative(Debug = "ignore")] Arc<dyn Fn(AntichainRef<T>) -> Antichain<T> + Send + Sync>,
    ),
    /// Allows one to express multiple read policies, taking the least of
    /// the resulting frontiers.
    Multiple(Vec<ReadPolicy<T>>),
}

impl ReadPolicy<mz_repr::Timestamp> {
    /// Creates a read policy that lags the write frontier by the indicated amount, rounded down to a multiple of that amount.
    ///
    /// The rounding down is done to reduce the number of changes the capability undergoes, with the thinking
    /// being that if you are ok with `lag`, then getting something between `lag` and `2 x lag` should be ok.
    pub fn lag_writes_by(lag: mz_repr::Timestamp) -> Self {
        Self::LagWriteFrontier(Arc::new(move |upper| {
            if upper.is_empty() {
                Antichain::from_elem(Timestamp::minimum())
            } else {
                // Subtract the lag from the time, and then round down to a multiple thereof to cut chatter.
                let mut time = upper[0];
                time = time.saturating_sub(lag);
                time = time.saturating_sub(time % lag);
                Antichain::from_elem(time)
            }
        }))
    }
}

impl<T: Timestamp> ReadPolicy<T> {
    pub fn frontier(&self, write_frontier: AntichainRef<T>) -> Antichain<T> {
        match self {
            ReadPolicy::ValidFrom(frontier) => frontier.clone(),
            ReadPolicy::LagWriteFrontier(logic) => logic(write_frontier),
            ReadPolicy::Multiple(policies) => {
                let mut frontier = Antichain::new();
                for policy in policies.iter() {
                    for time in policy.frontier(write_frontier).iter() {
                        frontier.insert(time.clone());
                    }
                }
                frontier
            }
        }
    }
}
